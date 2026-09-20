
//! Where a contract message and a hand-written `arvo_api` type meet.
//!
//! Both crates are foreign here, so the orphan rule forbids `From` impls;
//! these free functions are the one place the two shapes meet, and the engine
//! and the window both call them, so neither can drift from the other.
//!
//! Every shape still listed here is one the contract does not define yet.
//! When it does, the pair for it goes away — as the `View { kind, json }`
//! envelope did once every call named the message it answers with.

use crate::proto;
use arvo_api::{AttachmentView, RiskModelView, RuleView, RulesetFormView, RulesetView, StrategyView};

/// The most a message may carry, both ways. A study view holds curves,
/// ledgers and a search surface; tonic's default of four megabytes is not a
/// fit for a finding.
pub const MAX_MESSAGE_BYTES: usize = 64 << 20;

#[must_use]
pub fn attachment(view: AttachmentView) -> proto::research::Attachment {
    proto::research::Attachment {
        name: view.name,
        media_type: view.media_type,
        hash: view.hash,
        bytes: view.bytes,
        added_at: view.added_at,
    }
}

#[must_use]
pub fn attachment_view(wire: proto::research::Attachment) -> AttachmentView {
    AttachmentView {
        name: wire.name,
        media_type: wire.media_type,
        hash: wire.hash,
        bytes: wire.bytes,
        added_at: wire.added_at,
    }
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn size(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn params(pairs: Vec<(String, Vec<f64>)>) -> Vec<proto::research::Param> {
    pairs.into_iter().map(|(name, values)| proto::research::Param { name, values }).collect()
}

fn pairs(params: Vec<proto::research::Param>) -> Vec<(String, Vec<f64>)> {
    params.into_iter().map(|param| (param.name, param.values)).collect()
}

#[must_use]
pub fn strategy(view: StrategyView, ranks_a_set: bool) -> proto::research::Strategy {
    proto::research::Strategy {
        name: view.name,
        label: view.label,
        interval: view.interval,
        premise: view.premise,
        ranks_a_set,
        backtests: count(view.backtests),
    }
}

#[must_use]
pub fn strategy_view(wire: proto::research::Strategy) -> StrategyView {
    StrategyView {
        name: wire.name,
        label: wire.label,
        premise: wire.premise,
        interval: wire.interval,
        backtests: size(wire.backtests),
    }
}

#[must_use]
pub fn ruleset(view: RulesetView) -> proto::research::Ruleset {
    proto::research::Ruleset {
        path: view.path,
        name: view.name,
        label: view.label,
        rule: view.rule,
        interval: view.interval,
        searches: count(view.searches),
        problem: view.problem,
    }
}

#[must_use]
pub fn ruleset_view(wire: proto::research::Ruleset) -> RulesetView {
    RulesetView {
        path: wire.path,
        name: wire.name,
        label: wire.label,
        rule: wire.rule,
        interval: wire.interval,
        searches: size(wire.searches),
        problem: wire.problem,
    }
}

#[must_use]
pub fn ruleset_form(view: RulesetFormView) -> proto::research::RulesetForm {
    proto::research::RulesetForm {
        name: view.name,
        rule: view.rule,
        label: view.label,
        premise: view.premise,
        params: params(view.params),
    }
}

#[must_use]
pub fn ruleset_form_view(wire: proto::research::RulesetForm) -> RulesetFormView {
    RulesetFormView {
        name: wire.name,
        rule: wire.rule,
        label: wire.label,
        premise: wire.premise,
        params: pairs(wire.params),
    }
}

#[must_use]
pub fn rule(view: RuleView) -> proto::research::Rule {
    proto::research::Rule {
        name: view.name,
        label: view.label,
        premise: view.premise,
        interval: view.interval,
        fixed: view.fixed.into_iter().map(|(name, value)| proto::research::Fixed { name, value }).collect(),
        axes: params(view.axes),
        ranks_a_set: view.ranks_a_set,
        trades_options: view.trades_options,
    }
}

#[must_use]
pub fn rule_view(wire: proto::research::Rule) -> RuleView {
    RuleView {
        name: wire.name,
        label: wire.label,
        premise: wire.premise,
        interval: wire.interval,
        fixed: wire.fixed.into_iter().map(|fixed| (fixed.name, fixed.value)).collect(),
        axes: pairs(wire.axes),
        ranks_a_set: wire.ranks_a_set,
        trades_options: wire.trades_options,
    }
}

#[must_use]
pub fn risk_model(view: RiskModelView) -> proto::research::RiskModel {
    proto::research::RiskModel {
        path: view.path,
        exists: view.exists,
        model_json: view.model.to_string(),
        error: view.error,
    }
}

#[must_use]
pub fn risk_model_view(wire: proto::research::RiskModel) -> RiskModelView {
    RiskModelView {
        path: wire.path,
        exists: wire.exists,
        model: serde_json::from_str(&wire.model_json).unwrap_or_default(),
        error: wire.error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_risk_model_survives_the_round_trip() {
        // The model itself is still JSON inside the message: it is a
        // person's file, whose shape this build does not get to decide.
        let sent = RiskModelView { path: "p".into(), exists: false, model: serde_json::json!({"a": 1}), error: None };
        assert_eq!(risk_model_view(risk_model(sent.clone())), sent);
    }

    #[test]
    fn a_form_and_a_rule_survive_the_round_trip() {
        let form = RulesetFormView {
            name: "my_cross".into(),
            rule: "sma_cross".into(),
            label: "Mine".into(),
            premise: "".into(),
            params: vec![("fast".into(), vec![5.0]), ("slow".into(), vec![20.0, 50.0])],
        };
        assert_eq!(ruleset_form_view(ruleset_form(form.clone())), form);

        let view = RuleView {
            name: "sma_cross".into(),
            label: "Crossover".into(),
            premise: "p".into(),
            interval: "1d".into(),
            fixed: vec![("trade_size".into(), 100.0)],
            axes: vec![("fast".into(), vec![5.0, 10.0])],
            ranks_a_set: false,
            trades_options: false,
        };
        assert_eq!(rule_view(rule(view.clone())), view);

        let risk = RiskModelView {
            path: ".arvo/risk.json".into(),
            exists: true,
            model: serde_json::json!({"max_positions": 3}),
            error: None,
        };
        assert_eq!(risk_model_view(risk_model(risk.clone())), risk);
    }
}
