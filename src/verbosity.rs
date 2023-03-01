use clap::{builder::PossibleValue, ValueEnum};

#[derive(Clone, Debug, Eq, PartialEq, Copy)]
pub enum Verbosity {
    Silent,
    Quiet,
    Normal,
    Verbose,
    // VeryVerbose,
}

impl Default for Verbosity {
    fn default() -> Self {
        Self::Normal
    }
}

impl std::str::FromStr for Verbosity {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for variant in Self::value_variants() {
            if variant.to_possible_value().unwrap().matches(s, false) {
                return Ok(*variant);
            }
        }
        Err(format!("Invalid variant: {}", s))
    }
}

impl Verbosity {
    #[inline]
    pub fn bar(&self) -> bool {
        self == &Self::Normal || self == &Self::Verbose
    }

    #[inline]
    pub fn info(&self) -> bool {
        self == &Self::Normal || self == &Self::Verbose
    }

    #[inline]
    pub fn debug(&self) -> bool {
        self == &Self::Verbose
    }
}

impl ValueEnum for Verbosity {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::Silent, Self::Quiet, Self::Normal, Self::Verbose]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(match self {
            Self::Silent => PossibleValue::new("silent"),
            Self::Quiet => PossibleValue::new("quiet"),
            Self::Normal => PossibleValue::new("normal"),
            Self::Verbose => PossibleValue::new("verbose"),
        })
    }
}
