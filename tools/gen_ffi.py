#!/usr/bin/env python3
"""
Generate the Rust C-ABI shim skeleton from lz4's public headers.

Reads the LZ4LIB_API / LZ4FLIB_API declarations out of lib/*.h, converts each
one to a `#[no_mangle] pub extern "C"` stub, and cross-checks the result
against the symbol list exported by the real liblz4.a. Anything the headers
don't cover is reported so it can be handled by hand.

Usage:  python3 port/tools/gen_ffi.py <repo-root> <abi-symbol-list> <out.rs>
"""

import re
import sys
from pathlib import Path

HEADERS = ["lz4.h", "lz4hc.h", "lz4frame.h", "lz4file.h", "xxhash.h"]

API_MACROS = (
    "LZ4LIB_API",
    "LZ4LIB_STATIC_API",
    "LZ4FLIB_API",
    "LZ4FLIB_STATIC_API",
    "XXH_PUBLIC_API",
)

# C scalar base types -> Rust. `void` is special-cased (unit, or c_void behind
# a pointer). Anything absent here is treated as a struct/typedef name.
SCALARS = {
    "void": None,
    "char": "c_char",
    "signed char": "c_schar",
    "unsigned char": "c_uchar",
    "short": "c_short",
    "short int": "c_short",
    "unsigned short": "c_ushort",
    "int": "c_int",
    "signed": "c_int",
    "unsigned": "c_uint",
    "unsigned int": "c_uint",
    "long": "c_long",
    "long int": "c_long",
    "unsigned long": "c_ulong",
    "long long": "c_longlong",
    "unsigned long long": "c_ulonglong",
    "size_t": "usize",
    "float": "c_float",
    "double": "c_double",
}


def strip_comments(text):
    text = re.sub(r"/\*.*?\*/", " ", text, flags=re.S)
    text = re.sub(r"//[^\n]*", " ", text)
    # Drop preprocessor directives (honouring backslash continuations).
    # Without this, a line like `#define LZ4LIB_API ...` is a false start for
    # the declaration regex, which then swallows the next real declaration.
    text = re.sub(r"^[ \t]*#(?:[^\n\\]|\\.)*", " ", text, flags=re.M)
    return text


def c_type_to_rust(ctype):
    """Convert a C type string to a Rust type.

    Pointers are stripped first, then the remaining base name is mapped.
    Anything not a known C scalar is assumed to be a struct/typedef that
    crate::types declares.
    """
    t = " ".join(ctype.split())
    stars = t.count("*")

    base = " ".join(t.replace("*", " ").split())
    is_const = bool(re.match(r"^const\b", base))
    base = re.sub(r"\bconst\b", " ", base)
    base = re.sub(r"^\s*(?:struct|union|enum)\s+", " ", base)
    base = " ".join(base.split())

    if base in SCALARS:
        mapped = SCALARS[base]
        if mapped is None:  # void
            if stars == 0:
                return "()"
            mapped = "c_void"
    else:
        mapped = base or "c_void"

    if stars == 0:
        return mapped

    ptr = "*const " if is_const else "*mut "
    out = mapped
    for _ in range(stars):
        out = ptr + out
    return out


def split_params(param_str):
    """Split a parameter list on commas not nested in parens."""
    parts, depth, cur = [], 0, ""
    for ch in param_str:
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        if ch == "," and depth == 0:
            parts.append(cur)
            cur = ""
        else:
            cur += ch
    if cur.strip():
        parts.append(cur)
    return [p.strip() for p in parts if p.strip()]


def parse_param(p, idx):
    """Return (rust_name, rust_type) for one C parameter."""
    p = p.strip()
    if p == "void" or not p:
        return None
    if "..." in p:
        return None  # variadic: handled manually if it ever appears

    # Function pointer parameter -> opaque
    if "(*" in p:
        name = re.search(r"\(\s*\*\s*(\w+)\s*\)", p)
        return (name.group(1) if name else f"arg{idx}", "*mut c_void")

    # Array parameter decays to pointer
    p = re.sub(r"\[\s*\d*\s*\]", " *", p)

    m = re.match(r"^(.*?)(\w+)$", p)
    if not m:
        return (f"arg{idx}", c_type_to_rust(p))
    ctype, name = m.group(1), m.group(2)
    if not ctype.strip():  # bare type with no name, e.g. `int`
        return (f"arg{idx}", c_type_to_rust(p))
    return (name, c_type_to_rust(ctype))


def extract_decls(text):
    """Yield (ret_ctype, name, params_str) for each API declaration."""
    text = strip_comments(text)
    macro_re = "|".join(API_MACROS)
    # Match: MACRO <stuff> name ( params ) ;   -- across newlines
    pattern = re.compile(
        r"\b(?:" + macro_re + r")\b(?P<body>[^;{]*?\([^;{]*?\))\s*;",
        re.S,
    )
    for m in pattern.finditer(text):
        body = " ".join(m.group("body").split())
        # Split into "ret + name" and "(params)" at the LAST top-level paren group
        pm = re.match(r"^(?P<head>.*?)\s*\((?P<params>.*)\)$", body, re.S)
        if not pm:
            continue
        head, params = pm.group("head").strip(), pm.group("params").strip()
        hm = re.match(r"^(?P<ret>.*?)(?P<name>\w+)$", head, re.S)
        if not hm:
            continue
        ret, name = hm.group("ret").strip(), hm.group("name")
        if not ret:
            continue
        yield ret, name, params


def main():
    if len(sys.argv) != 4:
        print(__doc__)
        return 1
    root, abi_path, out_path = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])

    want = {s.strip() for s in abi_path.read_text().split() if s.strip()}

    found = {}
    for h in HEADERS:
        hp = root / "lib" / h
        if not hp.exists():
            continue
        for ret, name, params in extract_decls(hp.read_text(errors="replace")):
            found.setdefault(name, (ret, params, h))

    # xxhash symbols are exported namespaced as LZ4_XXH* (XXH_NAMESPACE=LZ4_)
    resolved = {}
    for sym in sorted(want):
        if sym in found:
            resolved[sym] = found[sym]
        elif sym.startswith("LZ4_XXH") and sym[4:] in found:
            resolved[sym] = found[sym[4:]]

    # Fallback: a few symbols are exported from the archive but declared
    # without any API macro (e.g. LZ4_compress_destSize_extState in lz4.h),
    # or not declared in a public header at all (the *_forceExtDict internals,
    # which live in lz4.c). Search for a bare declaration/definition by name.
    for sym in sorted(want - set(resolved)):
        for src in HEADERS + ["lz4.c", "lz4hc.c", "lz4frame.c", "lz4file.c"]:
            sp = root / "lib" / src
            if not sp.exists():
                continue
            body = strip_comments(sp.read_text(errors="replace"))
            m = re.search(
                r"(?<![\w])(?P<ret>(?:const\s+)?\w[\w\s\*]*?)\b"
                + re.escape(sym)
                + r"\s*\((?P<params>[^;{]*?)\)\s*[;{]",
                body,
                re.S,
            )
            if m:
                ret = " ".join(m.group("ret").split())
                # Strip storage-class/inline noise the header macros usually carry.
                ret = re.sub(r"^(?:static|extern|inline|LZ4_FORCE_INLINE)\s+", "", ret)
                if ret:
                    resolved[sym] = (ret, " ".join(m.group("params").split()), src)
                    break

    missing = sorted(want - set(resolved))

    lines = [
        "// AUTO-GENERATED by port/tools/gen_ffi.py -- do not edit by hand.",
        "//",
        "// One `extern \"C\"` stub per symbol exported by the original liblz4.a.",
        "// This is the linking skeleton: every name the original C test suite",
        "// asks for exists here, so `fuzzer`/`frametest` link. Bodies are filled",
        "// in module by module.",
        "#![allow(non_snake_case, unused_variables, dead_code, unused_imports)]",
        "",
        "use core::ffi::{c_char, c_double, c_float, c_int, c_long, c_longlong,",
        "                c_schar, c_short, c_uchar, c_uint, c_ulong, c_ulonglong,",
        "                c_ushort, c_void};",
        "",
        "use crate::types::*;",
        "",
    ]

    for sym in sorted(resolved):
        ret, params, hdr = resolved[sym]
        rust_ret = c_type_to_rust(ret)
        plist = []
        for i, p in enumerate(split_params(params)):
            parsed = parse_param(p, i)
            if parsed:
                plist.append(f"{parsed[0]}: {parsed[1]}")
        sig_ret = "" if rust_ret == "()" else f" -> {rust_ret}"
        lines.append(f"/// from lib/{hdr}")
        lines.append("#[no_mangle]")
        lines.append(
            f"pub extern \"C\" fn {sym}({', '.join(plist)}){sig_ret} {{"
        )
        lines.append(f"    unimplemented!(\"{sym}\")")
        lines.append("}")
        lines.append("")

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text("\n".join(lines))

    print(f"resolved : {len(resolved)}/{len(want)}")
    if missing:
        # Exit non-zero. A symbol the archive exports but we generate no stub
        # for links fine and then misbehaves at runtime, so this has to stop
        # the build rather than scroll past in the log.
        print(f"MISSING  : {len(missing)} exported symbols have no generated stub")
        for m in missing:
            print(f"    {m}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
