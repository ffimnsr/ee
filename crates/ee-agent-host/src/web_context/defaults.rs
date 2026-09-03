use super::*;

impl Default for AgentWebContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: WebSearchProvider::default(),
            provider_options: WebSearchProviderOptions::default(),
            search_endpoint: None,
            preapproved_hosts: BTreeSet::new(),
            limits: WebContextLimits::default(),
            provider_secret_reference: None,
            browser_run_account_id: None,
            browser_run_api_token_reference: None,
            browser_run_retry: BrowserRunRetryPolicy::default(),
            search_authorization: None,
            browser_run_api_token: None,
        }
    }
}
