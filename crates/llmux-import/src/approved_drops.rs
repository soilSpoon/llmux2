//! Exact buyer-visible spellings the owner approved for interruption because
//! no compliant free exact native lane exists. Keep this current-importer
//! table until WO-175 fallback-policy spellings carry explicit drop decisions.

pub(crate) const APPROVAL_SOURCE: &str = "WO-189 user decision: buyer-visible interruption";
pub(crate) const REASON: &str = "no compliant free exact native lane";

pub(crate) const USER_APPROVED_DROPS: &[&str] = &[
    "kimi-k2.6",
    "moonshotai/kimi-k2.6",
    "cloudflare-direct/moonshotai/kimi-k2.6",
    "cfp/moonshotai/kimi-k2.6",
    "cfpd/moonshotai/kimi-k2.6",
    "cloudflare-playground/moonshotai/kimi-k2.6",
    "ddgw/gpt-5.4-mini",
    "duckduckgo-web/ddgw/gpt-5.4-mini",
];

pub(crate) fn user_approved(name: &str) -> bool {
    USER_APPROVED_DROPS.contains(&name)
}
