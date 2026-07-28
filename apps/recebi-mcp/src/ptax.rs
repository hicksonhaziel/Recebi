use std::time::Duration;

use recebi_core::{
    PtaxDate, PtaxDecimal, PtaxEvidence, PtaxQuoteCandidate,
    limits::{MAX_PTAX_RESPONSE_BYTES, RPC_TIMEOUT_SECS},
    select_strict_same_day_quote,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use ureq::{Agent, config::Config};
use url::Url;

const BCB_PTAX_BASE_URL: &str = "https://olinda.bcb.gov.br/olinda/servico/PTAX/versao/v1/odata/";

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PtaxError {
    #[error("official PTAX source is temporarily unavailable")]
    Unavailable,
    #[error("official PTAX response exceeded the size limit")]
    ResponseTooLarge,
    #[error("official PTAX response failed validation")]
    MalformedResponse,
}

pub trait PtaxClient {
    fn quote(
        &self,
        operation_date: &PtaxDate,
        retrieved_at_unix_ms: i64,
    ) -> Result<Option<PtaxEvidence>, PtaxError>;
}

#[derive(Clone, Debug)]
pub struct HttpBcbPtax {
    agent: Agent,
    endpoint: Url,
}

impl HttpBcbPtax {
    /// Creates the pinned HTTPS-only BCB PTAX adapter.
    ///
    /// # Errors
    ///
    /// Fails only if the compile-time official endpoint is invalid.
    pub fn new() -> Result<Self, PtaxError> {
        let endpoint = Url::parse(BCB_PTAX_BASE_URL).map_err(|_| PtaxError::Unavailable)?;
        let config = Config::builder()
            .https_only(true)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(RPC_TIMEOUT_SECS)))
            .build();
        Ok(Self {
            agent: config.new_agent(),
            endpoint,
        })
    }

    fn request_url(&self, operation_date: &PtaxDate) -> Result<Url, PtaxError> {
        let mut url = self
            .endpoint
            .join("CotacaoDolarDia(dataCotacao=@dataCotacao)")
            .map_err(|_| PtaxError::Unavailable)?;
        url.query_pairs_mut()
            .append_pair(
                "@dataCotacao",
                &format!("'{}'", operation_date.as_bcb_parameter()),
            )
            .append_pair("$format", "json");
        Ok(url)
    }
}

impl PtaxClient for HttpBcbPtax {
    fn quote(
        &self,
        operation_date: &PtaxDate,
        retrieved_at_unix_ms: i64,
    ) -> Result<Option<PtaxEvidence>, PtaxError> {
        let url = self.request_url(operation_date)?;
        let mut response = self
            .agent
            .get(url.as_str())
            .call()
            .map_err(|_| PtaxError::Unavailable)?;
        if !response.status().is_success() {
            return Err(PtaxError::Unavailable);
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_PTAX_RESPONSE_BYTES as u64)
            .read_to_vec()
            .map_err(|error| {
                if error.to_string().contains("limit") {
                    PtaxError::ResponseTooLarge
                } else {
                    PtaxError::Unavailable
                }
            })?;
        parse_response(operation_date, retrieved_at_unix_ms, &bytes)
    }
}

#[derive(Deserialize)]
struct DailyEnvelope {
    value: Vec<DailyRow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DailyRow {
    cotacao_compra: Value,
    cotacao_venda: Value,
    data_hora_cotacao: String,
    #[serde(default)]
    tipo_boletim: Option<String>,
}

fn parse_response(
    operation_date: &PtaxDate,
    retrieved_at_unix_ms: i64,
    bytes: &[u8],
) -> Result<Option<PtaxEvidence>, PtaxError> {
    if bytes.len() > MAX_PTAX_RESPONSE_BYTES {
        return Err(PtaxError::ResponseTooLarge);
    }
    let envelope: DailyEnvelope =
        serde_json::from_slice(bytes).map_err(|_| PtaxError::MalformedResponse)?;
    let rows = envelope
        .value
        .into_iter()
        .map(|row| {
            if row.data_hora_cotacao.len() > 64 {
                return Err(PtaxError::MalformedResponse);
            }
            let date = row
                .data_hora_cotacao
                .get(0..10)
                .ok_or(PtaxError::MalformedResponse)?;
            Ok(PtaxQuoteCandidate {
                purchase: parse_decimal_value(&row.cotacao_compra)?,
                sale: parse_decimal_value(&row.cotacao_venda)?,
                quote_date: PtaxDate::parse(date).map_err(|_| PtaxError::MalformedResponse)?,
                bulletin_type: row.tipo_boletim,
                bulletin_timestamp: row.data_hora_cotacao,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let response_sha256 = format!("{:x}", Sha256::digest(bytes));
    select_strict_same_day_quote(operation_date, rows, retrieved_at_unix_ms, response_sha256)
        .map_err(|_| PtaxError::MalformedResponse)
}

fn parse_decimal_value(value: &Value) -> Result<PtaxDecimal, PtaxError> {
    let rendered = match value {
        Value::Number(number) => number.to_string(),
        Value::String(string) => string.clone(),
        _ => return Err(PtaxError::MalformedResponse),
    };
    PtaxDecimal::parse(&rendered).map_err(|_| PtaxError::MalformedResponse)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &[u8] = br#"{"@odata.context":"https://was-p.bcnet.bcb.gov.br/olinda/servico/PTAX/versao/v1/odata$metadata#_CotacaoDolarDia","value":[{"cotacaoCompra":5.11710,"cotacaoVenda":5.11770,"dataHoraCotacao":"2026-07-28 13:25:31.150278"}]}"#;

    #[test]
    fn parses_known_business_day_golden_and_hashes_exact_bytes() {
        let date = PtaxDate::parse("2026-07-28").expect("date");
        let evidence = parse_response(&date, 123, GOLDEN)
            .expect("response")
            .expect("quote");
        assert_eq!(evidence.purchase, "5.11710");
        assert_eq!(evidence.sale, "5.11770");
        assert_eq!(
            evidence.response_sha256,
            format!("{:x}", Sha256::digest(GOLDEN))
        );
        assert_eq!(evidence.retrieved_at_unix_ms, 123);
    }

    #[test]
    fn treats_weekends_holidays_and_future_empty_rows_as_unavailable() {
        for date in ["2026-07-25", "2026-12-25", "2099-12-31"] {
            let date = PtaxDate::parse(date).expect("date");
            assert_eq!(
                parse_response(&date, 123, br#"{"value":[]}"#).expect("empty"),
                None
            );
        }
    }

    #[test]
    fn rejects_date_mismatch_duplicate_rows_and_malformed_decimal() {
        let date = PtaxDate::parse("2026-07-28").expect("date");
        let mismatch = GOLDEN.to_vec();
        let mismatch = String::from_utf8(mismatch)
            .expect("UTF-8")
            .replace("2026-07-28", "2026-07-27");
        assert_eq!(
            parse_response(&date, 1, mismatch.as_bytes()),
            Err(PtaxError::MalformedResponse)
        );
        let row =
            br#"{"cotacaoCompra":5.1,"cotacaoVenda":5.2,"dataHoraCotacao":"2026-07-28 13:00:00"}"#;
        let duplicate = [br#"{"value":["#.as_slice(), row, b",", row, b"]}"].concat();
        assert_eq!(
            parse_response(&date, 1, &duplicate),
            Err(PtaxError::MalformedResponse)
        );
        let malformed = br#"{"value":[{"cotacaoCompra":5.1,"cotacaoVenda":"5e0","dataHoraCotacao":"2026-07-28 13:00:00"}]}"#;
        assert_eq!(
            parse_response(&date, 1, malformed),
            Err(PtaxError::MalformedResponse)
        );
    }

    #[test]
    fn rejects_oversized_response_before_json_parsing() {
        let date = PtaxDate::parse("2026-07-28").expect("date");
        assert_eq!(
            parse_response(&date, 1, &vec![b' '; MAX_PTAX_RESPONSE_BYTES + 1]),
            Err(PtaxError::ResponseTooLarge)
        );
    }

    #[test]
    fn pins_https_endpoint_zero_redirects_and_five_second_timeout() {
        let client = HttpBcbPtax::new().expect("client");
        assert_eq!(client.endpoint.scheme(), "https");
        assert_eq!(client.endpoint.host_str(), Some("olinda.bcb.gov.br"));
        let config = client.agent.config();
        assert!(config.https_only());
        assert_eq!(config.max_redirects(), 0);
        assert_eq!(
            config.timeouts().global,
            Some(Duration::from_secs(RPC_TIMEOUT_SECS))
        );
        let url = client
            .request_url(&PtaxDate::parse("2026-07-28").expect("date"))
            .expect("URL");
        assert!(url.as_str().contains("07-28-2026"));
        assert!(!url.as_str().contains("http%3A"));
    }
}
