//! file: `lib/lz4file.c` — the `FILE*` convenience layer over `frame`.
//!
//! Entry points live in `crate::ffi`. Nothing here may use `unsafe`; pointer
//! handling stays in the FFI shim so the port's unsafe surface is small and
//! countable.
//!
//! ## The split, and why it is where it is
//!
//! This layer is defined in terms of C stdio: `fread`, `fwrite`, `ferror` on a
//! caller-owned `FILE*`. Those cannot be called from a module that forbids
//! `unsafe`, and re-opening the handle as a Rust `File` is not an option — the
//! caller may hand us a stream they have already read from, and duplicating the
//! descriptor would desynchronise the file position.
//!
//! So the I/O stays in `ffi.rs`, behind the [`Io`] trait, and everything that
//! decides *what* to read or write lives here. That keeps the buffering rules —
//! which decide how input is chunked, and therefore where block boundaries fall
//! — in safe code next to the frame logic they mirror.
#![forbid(unsafe_code)]

use crate::frame::{self, BlockCompressMode, Cctx, Dctx, Preferences, Res};

/// The stdio operations this layer needs, implemented in `ffi.rs` over the
/// caller's `FILE*`.
pub trait Io {
    /// `fread`. Returns the number of bytes read; a short read means EOF or
    /// error, which [`Io::error`] disambiguates.
    fn read(&mut self, buf: &mut [u8]) -> usize;
    /// `fwrite`. Returns the number of bytes written.
    fn write(&mut self, buf: &[u8]) -> usize;
    /// `ferror` — distinguishes a real failure from a clean EOF.
    fn error(&mut self) -> bool;
}

/// `LZ4_readFile_t` (lz4file.c:52).
pub struct ReadFile {
    dctx: Dctx,
    src_buf: Vec<u8>,
    src_buf_next: usize,
    src_buf_size: usize,
}

impl ReadFile {
    /// `LZ4F_readOpen` + `readAndParseHeader` (lz4file.c:76, :119).
    ///
    /// Reads up to a full maximum-size header, parses it to learn the block
    /// size, and keeps whatever it over-read for the first `read` call. The
    /// minimum accepted is a header *plus an endmark*, so a truncated file is
    /// rejected here rather than midway through decoding.
    pub fn open(io: &mut dyn Io) -> Res<ReadFile> {
        let mut header = [0u8; frame::HEADER_SIZE_MAX];
        let bytes_read = io.read(&mut header);
        if bytes_read < frame::HEADER_SIZE_MIN + 4 {
            return Err(frame::Error::IoRead);
        }

        let mut dctx = Dctx::new();
        let (fi, consumed, _hint) = frame::get_frame_info(&mut dctx, &header[..bytes_read])?;

        let block_size = frame::get_block_size(fi.block_size_id)?;

        let leftover = bytes_read - consumed;
        let mut src_buf = vec![0u8; block_size.max(leftover)];
        src_buf[..leftover].copy_from_slice(&header[consumed..bytes_read]);

        Ok(ReadFile {
            dctx,
            src_buf,
            src_buf_next: 0,
            src_buf_size: leftover,
        })
    }

    /// `LZ4F_read` (lz4file.c:154) — fill `buf`, refilling from the file as
    /// needed. A short return means end of input, not an error.
    pub fn read(&mut self, io: &mut dyn Io, buf: &mut [u8]) -> Res<usize> {
        let mut total = 0usize;

        while total < buf.len() {
            let mut src_bytes = self.src_buf_size - self.src_buf_next;
            if src_bytes == 0 {
                let n = io.read(&mut self.src_buf);
                if n == 0 {
                    if io.error() {
                        return Err(frame::Error::IoRead);
                    }
                    break; // clean end of input
                }
                self.src_buf_size = n;
                self.src_buf_next = 0;
                src_bytes = n;
            }

            let from = self.src_buf_next;
            let p = frame::decompress(
                &mut self.dctx,
                &mut buf[total..],
                &self.src_buf[from..from + src_bytes],
                None,
                false,
            )?;

            self.src_buf_next += p.src_consumed;
            total += p.dst_written;

            // Neither side moved: the frame is finished, and looping again would
            // spin forever on the trailing bytes.
            if p.src_consumed == 0 && p.dst_written == 0 {
                break;
            }
        }

        Ok(total)
    }
}

/// `LZ4_writeFile_t` (lz4file.c:205).
pub struct WriteFile {
    cctx: Cctx,
    dst_buf: Vec<u8>,
    /// C's `maxWriteSize`: input is fed in chunks of exactly one block, which is
    /// what keeps `dstBuf` sized by `LZ4F_compressBound(blockSize)`.
    max_write_size: usize,
    /// C's `errCode`. Once set, `writeClose` skips the frame footer rather than
    /// finishing a frame it knows is broken.
    failed: bool,
}

impl WriteFile {
    /// `LZ4F_writeOpen` (lz4file.c:251) — size the buffers, then write the
    /// frame header immediately, so the file is a valid frame from byte 0.
    pub fn open(io: &mut dyn Io, prefs: Option<&Preferences>) -> Res<WriteFile> {
        let block_size_id = prefs.map(|p| p.frame_info.block_size_id).unwrap_or(0);
        let block_size = frame::get_block_size(block_size_id)?;

        let dst_buf_max = frame::compress_bound(block_size, prefs);
        let mut wf = WriteFile {
            cctx: Cctx::new(),
            dst_buf: vec![0u8; dst_buf_max],
            max_write_size: block_size,
            failed: false,
        };

        let header_size = wf.cctx.begin(&mut wf.dst_buf, None, None, prefs)?;
        if io.write(&wf.dst_buf[..header_size]) != header_size {
            return Err(frame::Error::IoWrite);
        }
        Ok(wf)
    }

    /// `LZ4F_write` (lz4file.c:305) — compress and write, one block-sized chunk
    /// at a time. Returns `buf.len()` on success, as C does.
    pub fn write(&mut self, io: &mut dyn Io, buf: &[u8]) -> Res<usize> {
        let mut sp = 0usize;
        while sp < buf.len() {
            let chunk = (buf.len() - sp).min(self.max_write_size);
            let c_size = match self.cctx.update(
                &mut self.dst_buf,
                &buf[sp..sp + chunk],
                BlockCompressMode::Compressed,
            ) {
                Ok(n) => n,
                Err(e) => {
                    self.failed = true;
                    return Err(e);
                }
            };
            if io.write(&self.dst_buf[..c_size]) != c_size {
                self.failed = true;
                return Err(frame::Error::IoWrite);
            }
            sp += chunk;
        }
        Ok(buf.len())
    }

    /// `LZ4F_writeClose` (lz4file.c:339) — finish the frame, unless an earlier
    /// write already failed.
    pub fn close(&mut self, io: &mut dyn Io) -> Res<()> {
        if self.failed {
            return Ok(());
        }
        let n = self.cctx.end(&mut self.dst_buf)?;
        if io.write(&self.dst_buf[..n]) != n {
            return Err(frame::Error::IoWrite);
        }
        Ok(())
    }
}
