//! The multiplexers workon knows by name.

/// Which program hosts the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MuxKind {
    Zellij,
    Tmux,
    Zmx,
}

impl MuxKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zellij => "zellij",
            Self::Tmux => "tmux",
            Self::Zmx => "zmx",
        }
    }

    /// `zellij`, `tmux`, or `zmx`. Anything else is an error the CLI can show.
    pub fn from_name(name: &str) -> Result<Self, String> {
        match name {
            "zellij" => Ok(Self::Zellij),
            "tmux" => Ok(Self::Tmux),
            "zmx" => Ok(Self::Zmx),
            other => Err(format!("unknown multiplexer '{other}'. workon supports zellij, tmux, and zmx.")),
        }
    }
}

/// clap value parser for `--mux`.
pub fn parse_mux(name: &str) -> Result<MuxKind, String> {
    MuxKind::from_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_known_name_reads_back() {
        for kind in [MuxKind::Zellij, MuxKind::Tmux, MuxKind::Zmx] {
            assert_eq!(MuxKind::from_name(kind.as_str()).unwrap(), kind);
        }
    }

    proptest::proptest! {
        /// A name parses exactly when it is one of the three, and then reads back as itself.
        #[test]
        fn parse_succeeds_iff_the_name_is_known(name in "[a-z]{0,8}") {
            let known = matches!(name.as_str(), "zellij" | "tmux" | "zmx");
            proptest::prop_assert_eq!(MuxKind::from_name(&name).is_ok(), known);
            if let Ok(kind) = MuxKind::from_name(&name) {
                proptest::prop_assert_eq!(kind.as_str(), name.as_str());
            }
        }
    }
}
