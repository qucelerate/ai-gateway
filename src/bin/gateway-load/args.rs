//! Flags as `--name value` or `--name=value`; each also read from the
//! environment as `LOAD_NAME` (dashes as underscores), so CI and the run
//! script can set them either way. A flag given on the command line wins,
//! then the environment, then the profile's default.

use std::{collections::HashMap, str::FromStr, time::Duration};

#[derive(Debug, Default)]
pub struct Flags {
    values: HashMap<String, Vec<String>>,
    defaults: HashMap<String, String>,
}

impl Flags {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut values: HashMap<String, Vec<String>> = HashMap::new();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let Some(name) = arg.strip_prefix("--") else {
                return Err(format!("unexpected argument {arg:?}"));
            };
            let (name, value) = match name.split_once('=') {
                Some((name, value)) => (name.to_owned(), value.to_owned()),
                None => {
                    let value = args
                        .next()
                        .ok_or_else(|| format!("--{name} needs a value"))?;
                    (name.to_owned(), value)
                }
            };
            values.entry(name).or_default().push(value);
        }
        Ok(Self {
            values,
            defaults: HashMap::new(),
        })
    }

    /// A profile's value for `name`, used when neither the command line nor
    /// the environment gives one.
    pub fn set_default(&mut self, name: &str, value: &str) {
        self.defaults.insert(name.to_owned(), value.to_owned());
    }

    fn env_name(name: &str) -> String {
        format!("LOAD_{}", name.replace('-', "_").to_ascii_uppercase())
    }

    pub fn get(&self, name: &str) -> Option<String> {
        self.values
            .get(name)
            .and_then(|values| values.last().cloned())
            .or_else(|| {
                std::env::var(Self::env_name(name))
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .or_else(|| self.defaults.get(name).cloned())
    }

    /// Every value given for a repeatable flag; from the environment, one
    /// per line (or `;`-separated).
    pub fn all(&self, name: &str) -> Vec<String> {
        if let Some(values) = self.values.get(name) {
            return values.clone();
        }
        std::env::var(Self::env_name(name))
            .map(|value| {
                value
                    .split(['\n', ';'])
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn string(&self, name: &str, default: &str) -> String {
        self.get(name).unwrap_or_else(|| default.to_owned())
    }

    pub fn required(&self, name: &str) -> Result<String, String> {
        self.get(name)
            .ok_or_else(|| format!("--{name} (or {}) is required", Self::env_name(name)))
    }

    pub fn parsed<T: FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.get(name) {
            None => Ok(default),
            Some(value) => value
                .trim()
                .parse()
                .map_err(|_| format!("--{name}: cannot parse {value:?}")),
        }
    }

    pub fn duration(&self, name: &str, default: Duration) -> Result<Duration, String> {
        match self.get(name) {
            None => Ok(default),
            Some(value) => {
                parse_duration(&value).ok_or_else(|| format!("--{name}: cannot parse {value:?}"))
            }
        }
    }

    pub fn range(&self, name: &str, default: (f64, f64)) -> Result<(f64, f64), String> {
        match self.get(name) {
            None => Ok(default),
            Some(value) => {
                parse_range(&value).ok_or_else(|| format!("--{name}: cannot parse {value:?}"))
            }
        }
    }
}

/// `250ms`, `30s`, `10m`, `4h` or a bare number of seconds.
pub fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let (number, scale) = if let Some(n) = value.strip_suffix("ms") {
        (n, 1e-3)
    } else if let Some(n) = value.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = value.strip_suffix('m') {
        (n, 60.0)
    } else if let Some(n) = value.strip_suffix('h') {
        (n, 3600.0)
    } else {
        (value, 1.0)
    };
    let seconds = number.trim().parse::<f64>().ok()? * scale;
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

/// `a..b` or a single value (`a..a`).
pub fn parse_range(value: &str) -> Option<(f64, f64)> {
    let (low, high) = match value.split_once("..") {
        Some((low, high)) => (low.trim().parse().ok()?, high.trim().parse().ok()?),
        None => {
            let single: f64 = value.trim().parse().ok()?;
            (single, single)
        }
    };
    (low <= high && low >= 0.0).then_some((low, high))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_take_both_spellings_and_the_last_value() {
        let flags = Flags::parse(
            [
                "--rate",
                "10",
                "--duration=30s",
                "--rate",
                "20",
                "--event",
                "a",
                "--event",
                "b",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(flags.parsed::<f64>("rate", 0.0).unwrap(), 20.0);
        assert_eq!(
            flags.duration("duration", Duration::ZERO).unwrap(),
            Duration::from_secs(30)
        );
        assert_eq!(flags.all("event"), ["a", "b"]);
        assert!(Flags::parse(["stray".to_owned()]).is_err());
        assert!(Flags::parse(["--rate".to_owned()]).is_err());
    }

    #[test]
    fn durations_and_ranges_parse() {
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("4h"), Some(Duration::from_secs(14_400)));
        assert_eq!(parse_duration("1.5"), Some(Duration::from_millis(1_500)));
        assert_eq!(parse_duration("-1s"), None);
        assert_eq!(parse_range("2..60"), Some((2.0, 60.0)));
        assert_eq!(parse_range("5"), Some((5.0, 5.0)));
        assert_eq!(parse_range("9..3"), None);
    }
}
