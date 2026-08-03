use lz4_rs::{block, frame, hc};

#[test]
fn block_helpers_round_trip_repeated_input() {
    let input = b"safe helper integration test ".repeat(512);
    let compressed_capacity = block::compress_bound(input.len() as i32) as usize;
    let mut compressed = vec![0; compressed_capacity];
    let compressed_len = block::compress_fast(
        &mut compressed,
        0..compressed_capacity,
        &block::Input::Separate(&input),
        block::LZ4_ACCELERATION_DEFAULT,
    )
    .expect("compression succeeds with compress_bound capacity");

    let decoded_capacity = input.len();
    let mut decoded = vec![0; decoded_capacity];
    let decoded_len = block::decompress_generic(
        &mut decoded,
        0..decoded_capacity,
        &block::Input::Separate(&compressed[..compressed_len]),
        false,
        0,
    )
    .expect("compressed data decompresses");

    assert_eq!(decoded_len, input.len());
    assert_eq!(decoded, input);
}

#[test]
fn block_helper_reports_insufficient_destination_capacity() {
    let input = b"input that cannot fit in an empty destination";
    let mut buffer = [];

    let result = block::compress_fast(
        &mut buffer,
        0..0,
        &block::Input::Separate(input),
        block::LZ4_ACCELERATION_DEFAULT,
    );

    assert_eq!(result, Err(block::Error::OutputTooSmall));
}

#[test]
fn frame_helpers_round_trip_with_checksums() {
    let input = b"frame helper integration test ".repeat(1024);
    let preferences = frame::Preferences {
        frame_info: frame::FrameInfo {
            content_checksum: true,
            block_checksum: true,
            content_size: input.len() as u64,
            ..frame::FrameInfo::default()
        },
        ..frame::Preferences::default()
    };
    let mut compressed = vec![0; frame::compress_frame_bound(input.len(), Some(&preferences))];
    let compressed_len = frame::compress_frame(
        &mut frame::Cctx::new(),
        &mut compressed,
        &input,
        None,
        Some(&preferences),
    )
    .expect("frame compression succeeds with frame bound capacity");

    let mut decoded = vec![0; input.len()];
    let progress = frame::decompress(
        &mut frame::Dctx::new(),
        &mut decoded,
        &compressed[..compressed_len],
        None,
        false,
    )
    .expect("frame decompression succeeds");

    assert_eq!(progress.src_consumed, compressed_len);
    assert_eq!(progress.dst_written, input.len());
    assert_eq!(progress.hint, 0);
    assert_eq!(decoded, input);
}

#[test]
fn hc_helper_clamps_and_classifies_levels() {
    assert_eq!(hc::clamp_level(-1), hc::LZ4HC_CLEVEL_DEFAULT);
    assert_eq!(hc::clamp_level(1), 1);
    assert_eq!(hc::clamp_level(99), hc::LZ4HC_CLEVEL_MAX);
    assert!(hc::is_mid_level(1));
    assert!(hc::is_mid_level(2));
    assert!(!hc::is_mid_level(3));
}
