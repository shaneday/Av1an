use std::error::Error;

use vergen::EmitBuilder;

fn main() -> Result<(), Box<dyn Error>> {

  // On OSX, the linker doesn't include homebrew path (needed to find libvapoursynth.a)
  if cfg!(target_os = "macos") {
    println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
  }

  EmitBuilder::builder()
    .git_sha(true)
    .git_commit_date()
    .cargo_debug()
    .cargo_target_triple()
    .rustc_semver()
    .rustc_llvm_version()
    .emit()?;
  Ok(())
}
