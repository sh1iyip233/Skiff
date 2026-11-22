// The palette in `ui/app.slint` is dark, so widgets (LineEdit, Switch, Spinner) must use the
// matching style variant instead of inheriting a light system theme.
fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile the Slint UI");
}
