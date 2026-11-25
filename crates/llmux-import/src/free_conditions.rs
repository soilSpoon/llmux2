//! Reviewed per-provider free-tier conditions. Every row cites an official
//! source; the footnote ids match `Shared/free-prospecting/free-providers.md`.
//! The table gives the emitted config the limits a provider documents so the
//! gateway throttles a free leg client-side instead of learning the cap from
//! a 429, marks the free-by-definition cases (OpenRouter `:free` variants,
//! Kilo anonymous access) free without an operator flag, and stamps
//! `expires_at` on time-boxed free offers so parked legs do not keep taking
//! requests after the promo ends.
//!
//! Only limits `QuotaSpec` can express are emitted: rpm/rpd/tpm/tpd plus
//! fixed anchored windows. Calendar-monthly caps and rows whose real window
//! shape or phase is unverified stay on the closest safe sliding bound —
//! each row's comment records the real limit; upgrading them to fixed
//! windows needs per-provider evidence of the reset phase.

/// One provider's documented free-tier conditions, keyed on the connection's
/// base URL host.
pub(super) struct Conditions {
    /// Exact base URL host the row applies to — substring matches would let
    /// `notfreemodel.in` or `openrouter.ai.evil.com` take another host's caps.
    pub host: &'static str,
    /// Only legs whose upstream model leaf ends with this suffix qualify
    /// (OpenRouter `*:free` variants).
    pub model_suffix: Option<&'static str>,
    /// Only legs whose upstream model leaf starts with this prefix qualify
    /// (Cohere's Command A family, per [C1]).
    pub model_prefix: Option<&'static str>,
    /// Only legs without a credential qualify (per-IP anonymous allowances).
    pub anonymous_only: bool,
    /// Whether matching alone marks the leg `billing = "free"` — only for
    /// plan-independent free offers. Key-gated providers whose free and paid
    /// plans share a host never set this; their free state needs
    /// `--free-providers` evidence.
    pub asserts_free: bool,
    /// Deployment `quota` fields to emit on a free leg. When `shared` names a
    /// family they become the `[[shared_quotas]]` spec instead — the
    /// documented limit is one account-wide allowance, not one per leg.
    pub quota: &'static [(&'static str, u32)],
    /// `[[shared_quotas]]` family name matching legs bind, `Some` when the
    /// row's cap is a per-account (or per-IP) allowance rather than per
    /// model. The gateway scopes the family by credential — or provider +
    /// base URL for anonymous legs — so two keys never share a window.
    pub shared: Option<&'static str>,
    /// RFC3339 `expires_at` for time-boxed free offers, stamped on free legs.
    pub expires_at: Option<&'static str>,
}

const TABLE: &[Conditions] = &[
    // OpenRouter :free variants without qualifying credits: 20 RPM / 50 RPD
    // [O1-O2]. The `:free` suffix is the provider's own free marker, so it
    // asserts free on its own.
    Conditions {
        host: "openrouter.ai",
        model_suffix: Some(":free"),
        model_prefix: None,
        anonymous_only: false,
        asserts_free: true,
        quota: &[("rpm", 20), ("rpd", 50)],
        shared: Some("openrouter-free"),
        expires_at: None,
    },
    // Kilo anonymous access: 200 requests/hour/IP [K2]. QuotaSpec has no
    // hourly window; rpm=3 admits at most 180/hour — the closest bound that
    // never exceeds the real cap. Keyed requests do not share the anonymous
    // allowance, so this is anonymous-only.
    Conditions {
        host: "ai-gateway.kilo.ai",
        model_suffix: None,
        model_prefix: None,
        anonymous_only: true,
        asserts_free: true,
        quota: &[("rpm", 3)],
        shared: Some("kilo-anonymous"),
        expires_at: None,
    },
    // freemodel.in Free plan: 60 RPM / 100K TPM [F2]. The 1M tokens/month
    // cap is not expressible. Plan-gated — a key could be on a paid plan, so
    // the free tag still needs --free-providers evidence.
    Conditions {
        host: "freemodel.in",
        model_suffix: None,
        model_prefix: None,
        anonymous_only: false,
        asserts_free: false,
        quota: &[("rpm", 60), ("tpm", 100_000)],
        shared: Some("freemodel-free"),
        expires_at: None,
    },
    // Cohere trial Command A family: 20 RPM [C1]. The 1,000 calls/month cap
    // is not expressible. [C1] rates the Command A family only, so the row
    // is scoped to `command-a*` leaves.
    Conditions {
        host: "api.cohere.ai",
        model_suffix: None,
        model_prefix: Some("command-a"),
        anonymous_only: false,
        asserts_free: false,
        quota: &[("rpm", 20)],
        shared: Some("cohere-trial"),
        expires_at: None,
    },
    // SambaNova free tier: 20 RPM / 20 RPD / 200K TPD [S1].
    Conditions {
        host: "api.sambanova.ai",
        model_suffix: None,
        model_prefix: None,
        anonymous_only: false,
        asserts_free: false,
        quota: &[("rpm", 20), ("rpd", 20), ("tpd", 200_000)],
        shared: Some("sambanova-free"),
        expires_at: None,
    },
    // packet.ai Token Factory: $0 until 2026-12-16, then paid [P1]. Rate
    // limit unverified — the free legs just expire with the promo.
    Conditions {
        host: "api.packet.ai",
        model_suffix: None,
        model_prefix: None,
        anonymous_only: false,
        asserts_free: false,
        quota: &[],
        shared: None,
        expires_at: Some("2026-12-16T23:59:59Z"),
    },
];

/// The conditions matching a leg, if any row applies.
pub(super) fn lookup(base_url: &str, model: &str, anonymous: bool) -> Option<&'static Conditions> {
    let host = crate::policy::host(base_url);
    let leaf = model.rsplit('/').next().unwrap_or(model);
    TABLE.iter().find(|cond| {
        host == cond.host
            && (!cond.anonymous_only || anonymous)
            && cond
                .model_suffix
                .is_none_or(|suffix| leaf.ends_with(suffix))
            && cond
                .model_prefix
                .is_none_or(|prefix| leaf.starts_with(prefix))
    })
}

/// Whether the row alone marks this leg free (plan-independent offers only).
pub(super) fn asserts_free(base_url: &str, model: &str, anonymous: bool) -> bool {
    lookup(base_url, model, anonymous).is_some_and(|cond| cond.asserts_free)
}
