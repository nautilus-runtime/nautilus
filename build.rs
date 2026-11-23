// This is the build script for the Nautilus project.
// It configures and compiles the Protocol Buffers definitions for the CRI (Container Runtime Interface) API.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure().compile(&["proto/cri-api/runtime/v1/api.proto"], &["proto"])?;
    Ok(())
}
