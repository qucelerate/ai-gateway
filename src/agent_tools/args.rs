//! Command-line words: positional arguments, `--name value` (or
//! `--name=value`) options, and a few boolean flags.

use std::collections::BTreeMap;

/// Options that take no value.
const FLAGS: &[&str] = &["all", "compact", "help", "verify", "yes"];

#[derive(Debug, Default, PartialEq)]
pub struct Args {
    pub positional: Vec<String>,
    pub options: BTreeMap<String, String>,
    pub flags: Vec<String>,
}

impl Args {
    pub fn parse(words: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut args = Args::default();
        let mut words = words.into_iter();
        let mut positional_only = false;
        while let Some(word) = words.next() {
            if positional_only || !word.starts_with("--") || word == "-" {
                args.positional.push(word);
                continue;
            }
            if word == "--" {
                positional_only = true;
                continue;
            }
            let name = &word[2..];
            if let Some((name, value)) = name.split_once('=') {
                args.insert(name, value.to_owned())?;
            } else if FLAGS.contains(&name) {
                args.flags.push(name.to_owned());
            } else {
                let value = words
                    .next()
                    .ok_or_else(|| format!("--{name} needs a value"))?;
                args.insert(name, value)?;
            }
        }
        Ok(args)
    }

    fn insert(&mut self, name: &str, value: String) -> Result<(), String> {
        if name.is_empty() {
            return Err("an option needs a name".into());
        }
        if self.options.insert(name.to_owned(), value).is_some() {
            return Err(format!("--{name} is given twice"));
        }
        Ok(())
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|flag| flag == name)
    }

    /// Removes and returns an option, so what is left can be checked for
    /// unknown options or passed on as query parameters.
    pub fn take(&mut self, name: &str) -> Option<String> {
        self.options.remove(name)
    }

    pub fn require(&mut self, name: &str) -> Result<String, String> {
        self.take(name)
            .ok_or_else(|| format!("--{name} is required"))
    }

    /// Fails on any option nobody took.
    pub fn finish(&self) -> Result<(), String> {
        match self.options.keys().next() {
            Some(name) => Err(format!("unknown option --{name}")),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Result<Args, String> {
        Args::parse(words.iter().map(|word| word.to_string()))
    }

    #[test]
    fn separates_positionals_options_and_flags() {
        let args = parse(&[
            "keys",
            "create",
            "--scopes",
            "inference",
            "--all",
            "sa-1",
            "--name=ci",
        ])
        .unwrap();
        assert_eq!(args.positional, ["keys", "create", "sa-1"]);
        assert_eq!(args.options["scopes"], "inference");
        assert_eq!(args.options["name"], "ci");
        assert!(args.flag("all"));
    }

    #[test]
    fn rejects_missing_values_and_repeats() {
        assert_eq!(parse(&["--name"]).unwrap_err(), "--name needs a value");
        assert_eq!(
            parse(&["--a=1", "--a", "2"]).unwrap_err(),
            "--a is given twice"
        );
    }

    #[test]
    fn treats_words_after_double_dash_as_positional() {
        let args = parse(&["chat", "--", "--not-an-option"]).unwrap();
        assert_eq!(args.positional, ["chat", "--not-an-option"]);
    }

    #[test]
    fn reports_options_nobody_took() {
        let mut args = parse(&["--name", "x", "--typo", "y"]).unwrap();
        args.take("name");
        assert_eq!(args.finish().unwrap_err(), "unknown option --typo");
    }
}
