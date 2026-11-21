// All editor code lives in the `ee_cli` library so the lib and bin targets do
// not each compile the same modules. Keep this file a thin entrypoint.
fn main() -> std::io::Result<()> {
    ee_cli::run()
}
