const PROTO_DIR: &str = "../proto";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed={}", PROTO_DIR);

    tonic_build::configure()
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .compile_protos(&[format!("{PROTO_DIR}/moat.proto")], &[PROTO_DIR])?;

    Ok(())
}
