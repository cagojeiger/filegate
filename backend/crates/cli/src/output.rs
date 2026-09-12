use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

use crate::{args::Output, error::Error, model::Data};

#[derive(Serialize)]
pub struct Envelope {
    pub schema_version: u8,
    pub ok: bool,
    pub command: &'static str,
    pub endpoint: Option<String>,
    pub data: Option<Data>,
    pub error: Option<Error>,
}

impl Envelope {
    pub fn applied(&self) -> bool {
        self.error
            .as_ref()
            .is_some_and(|error| error.outcome == "applied")
            || match &self.data {
                Some(Data::Installation { .. }) => true,
                Some(Data::Update(result)) => result.updated,
                _ => false,
            }
    }
}

pub fn emit(envelope: &Envelope, format: Output) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    match format {
        Output::Json => {
            serde_json::to_writer(&mut stdout, envelope)?;
            writeln!(stdout)?;
        }
        Output::Table => {
            if envelope.command == "status"
                && let Some(endpoint) = &envelope.endpoint
            {
                writeln!(stdout, "endpoint  {endpoint}")?;
            }
            if let Some(data) = &envelope.data {
                let value = serde_json::to_value(data)?;
                write!(stdout, "{}", table(&value))?;
            }
            if let Some(error) = &envelope.error {
                writeln!(
                    io::stderr().lock(),
                    "gscli: {} ({})",
                    error.message,
                    error.code
                )?;
            }
        }
    }
    stdout.flush()
}

// Tables are built from typed, public response models, never raw response objects.
fn table(value: &Value) -> String {
    match value {
        Value::Array(rows) if rows.is_empty() => "(empty)\n".to_owned(),
        Value::Array(rows) => {
            if let Some(Value::Object(first)) = rows.first() {
                let keys: Vec<_> = first.keys().collect();
                let mut cells = vec![keys.iter().map(|key| key.to_uppercase()).collect()];
                cells.extend(
                    rows.iter()
                        .map(|row| keys.iter().map(|key| cell(key, &row[*key])).collect()),
                );
                align(cells)
            } else {
                let mut cells = vec![vec!["ID".to_owned()]];
                cells.extend(rows.iter().map(|row| vec![cell("", row)]));
                align(cells)
            }
        }
        Value::Object(fields) => {
            let mut cells = Vec::new();
            for (key, value) in fields {
                if let Value::Object(nested) = value {
                    cells.extend(
                        nested
                            .iter()
                            .map(|(name, v)| vec![format!("{key}.{name}"), cell(name, v)]),
                    );
                } else {
                    cells.push(vec![key.clone(), cell(key, value)]);
                }
            }
            align(cells)
        }
        _ => format!("{}\n", cell("", value)),
    }
}

fn cell(key: &str, value: &Value) -> String {
    let text = if key.ends_with("_bytes") {
        value
            .as_i64()
            .map(bytes)
            .unwrap_or_else(|| value.to_string())
    } else {
        match value {
            Value::String(s) => s.clone(),
            Value::Null => "unknown".to_owned(),
            _ => value.to_string(),
        }
    };
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn align(rows: Vec<Vec<String>>) -> String {
    let mut widths = Vec::new();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if widths.len() <= i {
                widths.push(0);
            }
            if let Some(width) = widths.get_mut(i) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }
    rows.iter()
        .map(|row| {
            let line = row
                .iter()
                .enumerate()
                .map(|(i, cell)| {
                    let padding = widths
                        .get(i)
                        .copied()
                        .unwrap_or(0)
                        .saturating_sub(cell.chars().count());
                    format!("{cell}{}", " ".repeat(padding))
                })
                .collect::<Vec<_>>()
                .join("  ");
            format!("{}\n", line.trim_end())
        })
        .collect()
}

fn bytes(value: i64) -> String {
    let mut amount = value as f64;
    for unit in ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"] {
        if amount.abs() < 1024.0 || unit == "EiB" {
            return if unit == "B" {
                format!("{value} B")
            } else {
                format!("{amount:.1} {unit}")
            };
        }
        amount /= 1024.0;
    }
    format!("{value} B")
}

#[cfg(test)]
#[path = "output_tests.rs"]
mod tests;
