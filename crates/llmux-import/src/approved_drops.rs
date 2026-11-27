//! Exact buyer-visible spellings the owner approved for interruption because
//! no compliant free exact native lane exists. Keep this current-importer
//! table until WO-175 fallback-policy spellings carry explicit drop decisions.

pub(crate) struct ApprovedDrop {
    pub(crate) spelling: &'static str,
    pub(crate) approval: &'static str,
    pub(crate) reason: &'static str,
}

const WO189_APPROVAL: &str = "WO-189 user decision: buyer-visible interruption";
const WO189_REASON: &str = "no compliant free exact native lane";
const WO188_APPROVAL: &str = "WO-188 user constraint: free lanes only";
const PARKED_REASON: &str =
    "no adoptable free exact native lane; candidates parked/rejected under current evidence";

pub(crate) const USER_APPROVED_DROPS: &[ApprovedDrop] = &[
    ApprovedDrop {
        spelling: "kimi-k2.6",
        approval: WO189_APPROVAL,
        reason: WO189_REASON,
    },
    ApprovedDrop {
        spelling: "moonshotai/kimi-k2.6",
        approval: WO189_APPROVAL,
        reason: WO189_REASON,
    },
    ApprovedDrop {
        spelling: "cloudflare-direct/moonshotai/kimi-k2.6",
        approval: WO189_APPROVAL,
        reason: WO189_REASON,
    },
    ApprovedDrop {
        spelling: "cfp/moonshotai/kimi-k2.6",
        approval: WO189_APPROVAL,
        reason: WO189_REASON,
    },
    ApprovedDrop {
        spelling: "cfpd/moonshotai/kimi-k2.6",
        approval: WO189_APPROVAL,
        reason: WO189_REASON,
    },
    ApprovedDrop {
        spelling: "cloudflare-playground/moonshotai/kimi-k2.6",
        approval: WO189_APPROVAL,
        reason: WO189_REASON,
    },
    ApprovedDrop {
        spelling: "ddgw/gpt-5.4-mini",
        approval: WO189_APPROVAL,
        reason: WO189_REASON,
    },
    ApprovedDrop {
        spelling: "duckduckgo-web/ddgw/gpt-5.4-mini",
        approval: WO189_APPROVAL,
        reason: WO189_REASON,
    },
    ApprovedDrop {
        spelling: "gpt-5.6-luna",
        approval: WO188_APPROVAL,
        reason: PARKED_REASON,
    },
    ApprovedDrop {
        spelling: "duckduckgo-web/ddgw/gpt-5.6-luna",
        approval: WO188_APPROVAL,
        reason: PARKED_REASON,
    },
];

pub(crate) fn lookup(name: &str) -> Option<&'static ApprovedDrop> {
    USER_APPROVED_DROPS
        .iter()
        .find(|drop| drop.spelling == name)
}
