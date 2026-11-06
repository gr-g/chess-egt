pub(super) fn parse_byte_size(value: &str) -> Result<usize, String> {
    let normalized = value.to_ascii_lowercase();
    let (number, multiplier) = if let Some(number) = normalized.strip_suffix("kib") {
        (number, 1024)
    } else if let Some(number) = normalized.strip_suffix("mib") {
        (number, 1024 * 1024)
    } else if let Some(number) = normalized.strip_suffix("gib") {
        (number, 1024 * 1024 * 1024)
    } else if let Some(number) = normalized.strip_suffix('b') {
        (number, 1)
    } else {
        (normalized.as_str(), 1)
    };
    let bytes = number
        .parse::<usize>()
        .map_err(|_| format!("invalid byte size '{value}'"))?;
    bytes
        .checked_mul(multiplier)
        .ok_or_else(|| format!("byte size '{value}' is too large"))
}
