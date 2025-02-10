use awc::http::uri::PathAndQuery;
use crate::webserver::uri_template::UriTemplate;

pub fn uri_matches_pattern(path_and_query: &PathAndQuery, template: &UriTemplate) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use awc::http::uri::PathAndQuery;
    use crate::webserver::uri_template::uri::uri_matches_pattern;
    use crate::webserver::uri_template::UriTemplate;

    #[test]
    fn single_variable_pattern() {
        let pattern = "/path/{variable}";
        let uri = "/path/value";

        let actual = do_match(uri, pattern);
        assert_eq!(actual, Some("value".to_string()));
    }

    fn do_match(uri: &str, pattern: &str) -> Option<String> {
        let path_and_query: PathAndQuery = uri.parse().ok()?;
        let pattern = UriTemplate::new(pattern);

        uri_matches_pattern(&path_and_query, &pattern)
    }

}
