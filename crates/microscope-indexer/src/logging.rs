use std::io::Write;

use tokio::sync::mpsc::UnboundedSender;

const STRUCTURED_TARGET_PREFIX: &str = "microscope::";
const REDACTED_QUERY: &str = "?<redacted>";

pub fn init() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buffer, record| {
            if is_structured_target(record.target()) {
                writeln!(buffer, "{}", record.args())
            } else {
                writeln!(
                    buffer,
                    "{} {:<5} [{}] {}",
                    buffer.timestamp(),
                    record.level(),
                    record.target(),
                    redact_url_queries(&record.args().to_string())
                )
            }
        })
        .init();
}

/// Backfill mode: structured records are diverted to the sink for a
/// backdated Loki push instead of stdout, where Alloy would re-ingest them
/// with the current time. Everything else logs normally.
pub fn init_with_record_sink(sink: UnboundedSender<Option<String>>) {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(move |buffer, record| {
            if is_structured_target(record.target()) {
                let _ = sink.send(Some(record.args().to_string()));
                Ok(())
            } else {
                writeln!(
                    buffer,
                    "{} {:<5} [{}] {}",
                    buffer.timestamp(),
                    record.level(),
                    record.target(),
                    redact_url_queries(&record.args().to_string())
                )
            }
        })
        .init();
}

fn is_structured_target(target: &str) -> bool {
    target.starts_with(STRUCTURED_TARGET_PREFIX)
}

fn redact_url_queries(message: &str) -> String {
    let mut redacted = String::with_capacity(message.len());
    let mut rest = message;

    while let Some(scheme_at) = rest.find("://") {
        let authority_at = scheme_at + "://".len();
        let url_end = rest[authority_at..]
            .find(cannot_appear_in_url)
            .map_or(rest.len(), |offset| authority_at + offset);
        match rest[authority_at..url_end].find('?') {
            Some(query_at) => {
                redacted.push_str(&rest[..authority_at + query_at]);
                redacted.push_str(REDACTED_QUERY);
            }
            None => redacted.push_str(&rest[..url_end]),
        }
        rest = &rest[url_end..];
    }
    redacted.push_str(rest);
    redacted
}

fn cannot_appear_in_url(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '"' | '<' | '>' | '\\' | '^' | '`' | '{' | '|' | '}'
        )
}

#[cfg(test)]
mod tests {
    use super::{is_structured_target, redact_url_queries};

    #[test]
    fn identifies_only_microscope_structured_records() {
        assert!(is_structured_target("microscope::events"));
        assert!(is_structured_target("microscope::instructions"));
        assert!(!is_structured_target("carbon_log_metrics"));
    }

    /// The client repeats the endpoint once per wrapped error, so redacting only
    /// the first occurrence still ships the key.
    #[test]
    fn strips_the_api_key_from_every_endpoint_an_error_chain_repeats() {
        let redacted = redact_url_queries(
            "RPC poll failed: failed to read the confirmed RPC slot: error sending request for url (https://mainnet.helius-rpc.com/?api-key=SECRET): error sending request for url (https://mainnet.helius-rpc.com/?api-key=SECRET): connection error",
        );

        assert!(!redacted.contains("SECRET"), "{redacted}");
        assert_eq!(redacted.matches("?<redacted>").count(), 2, "{redacted}");
        assert!(
            redacted.contains("https://mainnet.helius-rpc.com/?<redacted>"),
            "the host has to survive so the failing endpoint stays identifiable: {redacted}"
        );
    }

    /// Redaction runs on every line, so it has to leave the ordinary ones alone.
    #[test]
    fn leaves_urls_without_a_query_and_plain_messages_untouched() {
        let explorer = "decoded https://explorer.solana.com/tx/5xY, slot 42";
        assert_eq!(redact_url_queries(explorer), explorer);

        let plain = "RPC polling monitors 11111111111111111111111111111111";
        assert_eq!(redact_url_queries(plain), plain);
    }

    /// Sub-delimiters are legal in both a path and a query, so any of them
    /// treated as the end of the URL stops redaction short and copies the rest
    /// of the query, credential included, into the line verbatim. Only a
    /// character RFC 3986 forbids outright can end the scan safely.
    #[test]
    fn redacts_past_punctuation_that_is_legal_inside_a_url() {
        for message in [
            "error sending request for url (https://host/rpc?filter=(active)&api-key=SECRET): boom",
            "error sending request for url (https://host/rpc,v2?api-key=SECRET): boom",
            "error sending request for url (https://host/rpc?ids=1,2&api-key=SECRET): boom",
            "error sending request for url (https://host/rpc;v2?api-key=SECRET): boom",
            "error sending request for url (https://host/rpc?a=1'2&api-key=SECRET): boom",
        ] {
            let redacted = redact_url_queries(message);
            assert!(!redacted.contains("SECRET"), "{redacted}");
        }
    }

    /// Trailing punctuation is indistinguishable from part of the query, so it
    /// is absorbed. Losing it is the safe direction; keeping it means guessing.
    #[test]
    fn absorbs_trailing_punctuation_rather_than_risk_stopping_early() {
        assert_eq!(
            redact_url_queries("error for url (https://host/?api-key=SECRET): boom"),
            "error for url (https://host/?<redacted> boom"
        );
    }

    /// A URL ending the line has no terminator to stop the scan.
    #[test]
    fn redacts_a_query_that_runs_to_the_end_of_the_message() {
        assert_eq!(
            redact_url_queries("connecting to https://example.com/rpc?token=SECRET"),
            "connecting to https://example.com/rpc?<redacted>"
        );
    }
}
