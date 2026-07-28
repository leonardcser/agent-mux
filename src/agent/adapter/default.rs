use super::ProviderAdapter;

pub(super) struct DefaultAdapter;

impl ProviderAdapter for DefaultAdapter {}

pub(super) static DEFAULT_ADAPTER: DefaultAdapter = DefaultAdapter;
