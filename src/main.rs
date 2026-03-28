use eyre::Result;

fn main() -> Result<()> {
    println!("codetracer-evm-recorder v{}", env!("CARGO_PKG_VERSION"));
    // TODO: CLI argument parsing (M2)
    Ok(())
}
