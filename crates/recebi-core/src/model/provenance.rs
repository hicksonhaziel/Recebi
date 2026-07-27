use serde::{Deserialize, Serialize};

/// How Recebi knows a value; chat text is never a provenance source.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    ChainVerified,
    BcbVerified,
    OperatorSupplied,
    Derived,
}
