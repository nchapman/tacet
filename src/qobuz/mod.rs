pub mod auth;
pub mod client;
pub mod discover;
pub mod types;

pub use client::{Client, Config, sign_file_url};
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_file_url_matches_qobuz_formula() {
        let sig = sign_file_url(123456, 27, "stream", 1700000000, "secret");
        let expected =
            md5_hex("trackgetFileUrlformat_id27intentstreamtrack_id1234561700000000secret");
        assert_eq!(sig, expected);
    }

    #[test]
    fn quality_round_trip() {
        assert_eq!(parse_quality("mp3_320").unwrap(), FORMAT_MP3_320);
        assert_eq!(parse_quality("flac_16_44").unwrap(), FORMAT_FLAC_16_44);
        assert_eq!(parse_quality("flac_24_96").unwrap(), FORMAT_FLAC_24_96);
        assert_eq!(parse_quality("flac_24_192").unwrap(), FORMAT_FLAC_192);
        assert!(parse_quality("dsd").is_err());
        assert_eq!(quality_label(FORMAT_FLAC_24_96), "flac_24_96");
    }

    #[test]
    fn credentials_export_restore_logout() {
        let c = Client::new(Config {
            app_id: "100".into(),
            app_secret: "sec".into(),
            auth_token: "tok".into(),
            ..Default::default()
        });
        assert!(c.is_connected());
        let creds = c.export_credentials();
        assert_eq!(creds.app_id, "100");
        assert_eq!(creds.auth_token, "tok");

        let c2 = Client::new(Config::default());
        assert!(!c2.is_connected());
        c2.restore_credentials(creds);
        assert!(c2.is_connected());

        let st = c2.logout();
        assert!(!st.authenticated);
        assert!(!c2.is_connected());
    }
}
