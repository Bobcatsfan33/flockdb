// Compile the C interpose glue into this cdylib. The glue carries the __DATA,__interpose section
// dyld reads to redirect `pread`/`read`/`open`/... in the DuckDB process; the actual page serving
// is `flock_serve`, defined in Rust in the same dylib, which the glue calls directly.
fn main() {
    println!("cargo:rerun-if-changed=faultglue.c");
    cc::Build::new()
        .file("faultglue.c")
        .opt_level(2)
        .compile("faultglue");
    // The interpose atoms are `static` (local) symbols, so nothing in the archive member satisfies
    // an undefined symbol and the linker would drop the whole object — taking the __interpose
    // section with it. `flock_faultglue_anchor` is a global the Rust side references (lib.rs), which
    // forces the object in; the interpose atoms then survive dead-strip on their `used` attribute
    // (macOS marks `used` symbols no-dead-strip). This is the standard DYLD_INTERPOSE combination.
}
