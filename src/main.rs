use eyre::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "Usage: codetracer-evm-recorder <rpc-url> <tx-hash> [--output-dir <dir>]"
        );
        eprintln!(
            "       codetracer-evm-recorder --artifacts <path> --structlog <path> [--output-dir <dir>]"
        );
        std::process::exit(1);
    }
    // TODO: full CLI implementation
    Ok(())
}
