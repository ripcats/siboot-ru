use std::fmt;

#[derive(Debug)]
pub struct Fail {
    pub advice: String,
    pub raw: String,
    pub by_user: bool,
    pub known: bool,
}

impl Fail {
    pub fn new(advice: impl Into<String>, raw: impl Into<String>) -> Self {
        Self {
            advice: advice.into(),
            raw: raw.into(),
            by_user: false,
            known: false,
        }
    }

    pub fn known(advice: impl Into<String>) -> Self {
        Self {
            known: true,
            ..Self::new(advice, "")
        }
    }

    pub fn by_user(advice: impl Into<String>) -> Self {
        Self {
            by_user: true,
            ..Self::new(advice, "")
        }
    }
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.advice)
    }
}

pub type Result<T> = std::result::Result<T, Fail>;

pub fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(&format!(": {s}"));
        source = s.source();
    }
    out
}
