/// Format a byte count for human consumption. Always KB at one decimal, since
/// we only use this for `.claude.json` size deltas.
pub fn kb(bytes: usize) -> String {
    format!("{:.1} KB", bytes as f64 / 1024.0)
}
