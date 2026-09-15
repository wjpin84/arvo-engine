use super::*;
use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveDateTime};

use crate::{CostModel, Trade};

fn day(d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 9, d).expect("valid")
}

fn at(d: u32, hour: u32, minute: u32, second: u32) -> NaiveDateTime {
    day(d).and_hms_opt(hour, minute, second).expect("valid")
}

fn model() -> RiskModel {
    RiskModel {
        max_position_fraction: Some(1.0),
        ..RiskModel::default()
    }
}

fn gate(model: RiskModel) -> RiskGate {
    RiskGate::new(model, 2_000.0, day(9))
}

fn proposal(instrument: &str) -> Proposal {
    Proposal {
        instrument: instrument.to_owned(),
        proposer: "technical".to_owned(),
        signalled_at: at(9, 14, 30, 0),
        reference_price: 100.0,
        stop_distance: Some(2.0),
        desired_quantity: None,
        opens_short: false,
    }
}

/// The instant a proposal is judged at: the moment it was signalled.
fn immediately(proposal: &Proposal) -> NaiveDateTime {
    proposal.signalled_at
}

struct Fixed(f64);
impl Correlations for Fixed {
    fn between(&self, _: &str, _: &str) -> Option<f64> {
        Some(self.0)
    }
}

struct Unknown;
impl Correlations for Unknown {
    fn between(&self, _: &str, _: &str) -> Option<f64> {
        None
    }
}

#[test]
fn an_ordinary_proposal_is_sized_and_accepted() {
    let gate = gate(model());
    let proposal = proposal("MSFT.RH");
    let Decision::Accept { quantity } = gate.propose(&proposal, immediately(&proposal), None)
    else {
        panic!("a first proposal on an untouched account should pass");
    };
    // $2,000 at $100, capped at one whole account.
    assert!((quantity - 20.0).abs() < 1e-9);
}

#[test]
fn a_signal_older_than_the_window_is_refused_rather_than_traded_late() {
    // Entering a momentum break late is a loss, not a delay. This is the
    // control that stops an LLM or an alert pipeline trading a market that
    // has already moved on.
    let gate = gate(model());
    let proposal = proposal("MSFT.RH");
    let late = proposal.signalled_at + chrono::Duration::milliseconds(501);

    assert_eq!(
        gate.propose(&proposal, late, None),
        Decision::Reject(Rejection::Stale {
            age_ms: 501,
            limit_ms: 500,
        })
    );
}

#[test]
fn a_signal_inside_the_window_still_passes() {
    let gate = gate(model());
    let proposal = proposal("MSFT.RH");
    let just_in_time = proposal.signalled_at + chrono::Duration::milliseconds(499);
    assert!(matches!(
        gate.propose(&proposal, just_in_time, None),
        Decision::Accept { .. }
    ));
}

#[test]
fn the_staleness_window_is_configurable_because_horizons_differ() {
    // A daily-rebalance proposal is not stale at five seconds and a scalp
    // is stale at fifty milliseconds.
    let gate = gate(model()).with_max_signal_age_ms(50);
    let proposal = proposal("MSFT.RH");
    let late = proposal.signalled_at + chrono::Duration::milliseconds(51);
    assert!(matches!(
        gate.propose(&proposal, late, None),
        Decision::Reject(Rejection::Stale { .. })
    ));
}

#[test]
fn two_proposers_cannot_both_open_the_same_instrument() {
    // The hazard the gate exists for. Each path sizes against its own idea
    // of the account, so without one authority this is two full positions
    // in one name.
    let mut gate = gate(model());
    let first = proposal("MSFT.RH");
    let Decision::Accept { quantity } = gate.propose(&first, immediately(&first), None) else {
        panic!("first proposal passes");
    };
    gate.opened("MSFT.RH", quantity, 100.0, day(9));

    let mut second = proposal("MSFT.RH");
    second.proposer = "alert".to_owned();
    assert_eq!(
        gate.propose(&second, immediately(&second), None),
        Decision::Reject(Rejection::AlreadyHeld { quantity: 20.0 })
    );
}

#[test]
fn the_concurrent_position_cap_counts_what_the_account_holds() {
    let mut gate = gate(RiskModel {
        max_concurrent_positions: Some(2),
        ..model()
    });
    gate.opened("MSFT.RH", 5.0, 100.0, day(9));
    gate.opened("AAPL.RH", 5.0, 100.0, day(9));

    let third = proposal("NVDA.RH");
    assert_eq!(
        gate.propose(&third, immediately(&third), None),
        Decision::Reject(Rejection::TooManyPositions { held: 2, limit: 2 })
    );
}

#[test]
fn the_kill_switch_refuses_everything_until_it_is_released() {
    let mut gate = gate(model());
    let proposal = proposal("MSFT.RH");
    assert!(matches!(
        gate.propose(&proposal, immediately(&proposal), None),
        Decision::Accept { .. }
    ));

    gate.kill("operator pulled it");
    assert_eq!(
        gate.propose(&proposal, immediately(&proposal), None),
        Decision::Reject(Rejection::Halted {
            reason: "operator pulled it".to_owned()
        })
    );

    assert!(gate.release(), "a manual halt is a person's to lift");
    assert!(gate.halted().is_none());
    assert!(matches!(
        gate.propose(&proposal, immediately(&proposal), None),
        Decision::Accept { .. }
    ));
}

#[test]
fn releasing_does_not_lift_a_drawdown_halt() {
    // The drawdown halt is permanent for the session on purpose: a gate
    // that stopped trading cannot recover the equity that would let it
    // resume. One release path that lifted both would quietly undo that.
    let mut gate = gate(RiskModel {
        max_drawdown: Some(0.10),
        ..model()
    });
    gate.mark(1_700.0);
    assert!(gate.halted().is_some(), "15% below a 10% limit");

    assert!(!gate.release(), "nobody chose this halt, so nobody can unchoose it");
    assert!(gate.halted().is_some());
}

#[test]
fn arming_on_top_of_a_drawdown_halt_does_not_make_it_releasable() {
    // The hole this is shaped to close: press the button once to arm, once
    // more to release, and an account that breached its drawdown limit is
    // trading again without anything having recovered.
    let mut gate = gate(RiskModel {
        max_drawdown: Some(0.10),
        ..model()
    });
    gate.mark(1_700.0);
    let breach = gate.halted().expect("halted").to_owned();

    gate.kill("operator pulled it");
    assert_eq!(
        gate.halted(),
        Some(breach.as_str()),
        "the first halt stands and keeps its reason"
    );
    assert!(!gate.release());
    assert!(gate.halted().is_some());
}

#[test]
fn the_daily_loss_limit_stops_trading_for_the_day() {
    let mut gate = gate(RiskModel {
        max_daily_loss: Some(0.02),
        ..model()
    });
    gate.opened("MSFT.RH", 20.0, 100.0, day(9));
    // $40 lost against a $40 limit on $2,000.
    gate.closed("MSFT.RH", -40.0, day(9));

    let next = proposal("AAPL.RH");
    assert_eq!(
        gate.propose(&next, immediately(&next), None),
        Decision::Reject(Rejection::DailyLossLimit {
            lost: 40.0,
            limit: 40.0,
        })
    );
}

#[test]
fn the_daily_limit_lifts_the_next_day_and_the_drawdown_halt_does_not() {
    // The distinction between the two controls. One is a rule about how bad
    // a day may get; the other is a conclusion that the idea is wrong.
    let mut gate = gate(RiskModel {
        max_daily_loss: Some(0.02),
        max_drawdown: Some(0.10),
        ..model()
    });
    gate.opened("MSFT.RH", 20.0, 100.0, day(9));
    gate.closed("MSFT.RH", -40.0, day(9));

    let tomorrow = Proposal {
        signalled_at: at(10, 14, 30, 0),
        ..proposal("AAPL.RH")
    };
    assert!(
        matches!(
            gate.propose(&tomorrow, immediately(&tomorrow), None),
            Decision::Accept { .. }
        ),
        "a new day resets the daily limit"
    );

    // Now breach the drawdown halt instead.
    gate.mark(1_700.0);
    assert!(gate.halted().is_some());
    let after = Proposal {
        signalled_at: at(11, 14, 30, 0),
        ..proposal("NVDA.RH")
    };
    assert!(
        matches!(
            gate.propose(&after, immediately(&after), None),
            Decision::Reject(Rejection::Halted { .. })
        ),
        "the halt does not lift with the date"
    );
}

#[test]
fn the_drawdown_halt_sees_open_positions_move_against_the_account() {
    // A halt counting only realised losses would let an account fall to
    // nothing while holding.
    let mut gate = gate(RiskModel {
        max_drawdown: Some(0.10),
        ..model()
    });
    gate.opened("MSFT.RH", 20.0, 100.0, day(9));
    assert!(gate.halted().is_none());
    gate.mark(1_799.0);
    assert!(gate.halted().is_some(), "unrealised losses count");
}

#[test]
fn correlated_names_count_as_one_bet() {
    let mut gate = gate(RiskModel {
        correlation_cap: Some(CorrelationCap {
            above: 0.8,
            max_positions: 1,
        }),
        ..model()
    });
    gate.opened("QQQ.RH", 5.0, 100.0, day(9));

    let tqqq = proposal("TQQQ.RH");
    let Decision::Reject(Rejection::Correlated { with, .. }) =
        gate.propose(&tqqq, immediately(&tqqq), Some(&Fixed(0.97)))
    else {
        panic!("a 0.97-correlated name is the same bet");
    };
    assert_eq!(with, vec!["QQQ.RH".to_owned()]);
}

#[test]
fn an_uncorrelated_name_is_a_new_bet_and_passes() {
    let mut gate = gate(RiskModel {
        correlation_cap: Some(CorrelationCap {
            above: 0.8,
            max_positions: 1,
        }),
        ..model()
    });
    gate.opened("QQQ.RH", 5.0, 100.0, day(9));

    let gold = proposal("GLD.RH");
    assert!(matches!(
        gate.propose(&gold, immediately(&gold), Some(&Fixed(0.05))),
        Decision::Accept { .. }
    ));
}

#[test]
fn a_cap_that_cannot_be_evaluated_refuses_rather_than_waving_through() {
    // An unenforceable limit that silently allows everything is worse than
    // no limit, because the operator believes they have one.
    let mut gate = gate(RiskModel {
        correlation_cap: Some(CorrelationCap {
            above: 0.8,
            max_positions: 1,
        }),
        ..model()
    });
    gate.opened("QQQ.RH", 5.0, 100.0, day(9));
    let next = proposal("TQQQ.RH");

    assert!(matches!(
        gate.propose(&next, immediately(&next), None),
        Decision::Reject(Rejection::CorrelationUnknown { .. })
    ));
    assert!(
        matches!(
            gate.propose(&next, immediately(&next), Some(&Unknown)),
            Decision::Reject(Rejection::CorrelationUnknown { .. })
        ),
        "a source that does not know this pair is not a source that says zero"
    );
}

#[test]
fn no_cap_configured_needs_no_correlation_source() {
    let gate = gate(model());
    let next = proposal("MSFT.RH");
    assert!(matches!(
        gate.propose(&next, immediately(&next), None),
        Decision::Accept { .. }
    ));
}

#[test]
fn risk_sizing_without_a_stop_is_refused_rather_than_invented() {
    let gate = gate(RiskModel {
        risk_per_trade: Some(0.01),
        stop_atr_multiple: Some(2.0),
        ..model()
    });
    let mut naked = proposal("MSFT.RH");
    naked.stop_distance = None;
    assert_eq!(
        gate.propose(&naked, immediately(&naked), None),
        Decision::Reject(Rejection::NoStop)
    );
}

#[test]
fn a_tight_stop_does_not_buy_more_than_the_account() {
    // The bug the position cap exists for: size is capital-at-risk over
    // stop distance, so a tight stop asks for a bigger position. On
    // five-minute bars this asks for several times the account.
    let gate = gate(RiskModel {
        risk_per_trade: Some(0.01),
        stop_atr_multiple: Some(2.0),
        max_position_fraction: Some(1.0),
        ..model()
    });
    let mut scalp = proposal("MSFT.RH");
    scalp.stop_distance = Some(0.01);

    let Decision::Accept { quantity } = gate.propose(&scalp, immediately(&scalp), None) else {
        panic!("it should still trade, just not for more than it has");
    };
    assert!(
        quantity * scalp.reference_price <= 2_000.0,
        "sized {quantity} at {} = {}, more than the account",
        scalp.reference_price,
        quantity * scalp.reference_price
    );
}

/// One decision against a $100k account holding nothing, wanting as much
/// of a $100 stock as the cap allows.
fn sized(spendable: Option<f64>, costs: Option<&CostModel>) -> f64 {
    let positions = BTreeMap::new();
    let greedy = Proposal {
        desired_quantity: Some(1_000_000.0),
        reference_price: 100.0,
        ..proposal("MSFT.RH")
    };
    let decision = decide(
        &model(),
        &AccountState {
            positions: &positions,
            realised_today: 0.0,
            starting_cash: 100_000.0,
            equity: 100_000.0,
            day_trades_used: 0,
            halted: None,
            spendable,
        },
        &greedy,
        greedy.signalled_at,
        i64::MAX,
        None,
        costs,
    );
    match decision {
        Decision::Accept { quantity } => quantity,
        Decision::Reject(why) => panic!("refused: {why:?}"),
    }
}

#[test]
fn the_cap_is_what_the_account_can_pay_all_in_not_on_price_alone() {
    // 1,000 shares at $100 is the whole account. With slippage and
    // commission on top it is more than the whole account, and the venue
    // refused it — so the cap counts them.
    assert!((sized(None, None) - 1_000.0).abs() < 1e-9, "no costs, as before");
    let costs = CostModel::proportional(10.0, 10.0);
    let all_in = sized(None, Some(&costs));
    assert!((all_in - 998.0).abs() < 1e-9, "{all_in}");
    assert!(all_in * 100.0 * 1.001 * 1.001 <= 100_000.0);
}

/// As [`sized`], for an option contract at `premium` a share.
fn sized_option(premium: f64, wanted: f64, costs: Option<&CostModel>) -> Decision {
    let positions = BTreeMap::new();
    let proposal = Proposal {
        desired_quantity: Some(wanted),
        reference_price: premium,
        ..proposal("SPY250912P00640000.AOPT")
    };
    decide(
        &model(),
        &AccountState {
            positions: &positions,
            realised_today: 0.0,
            starting_cash: 10_000.0,
            equity: 10_000.0,
            day_trades_used: 0,
            halted: None,
            spendable: None,
        },
        &proposal,
        proposal.signalled_at,
        i64::MAX,
        None,
        costs,
    )
}

/// One decision against a $100k account holding `positions`, with $100k
/// spendable.
fn decided(positions: BTreeMap<String, Position>, proposal: &Proposal) -> Decision {
    decide(
        &model(),
        &AccountState {
            positions: &positions,
            realised_today: 0.0,
            starting_cash: 100_000.0,
            equity: 100_000.0,
            day_trades_used: 0,
            halted: None,
            spendable: Some(100_000.0),
        },
        proposal,
        proposal.signalled_at,
        i64::MAX,
        None,
        None,
    )
}

#[test]
fn a_short_put_is_sized_by_its_strike_not_its_premium() {
    let sale = Proposal {
        reference_price: 2.0,
        desired_quantity: Some(500.0),
        opens_short: true,
        ..proposal("SPY250912P00300000.AOPT")
    };
    // $30,000 a contract against $100k: three, not the 500 units $2 a share
    // would suggest.
    assert_eq!(decided(BTreeMap::new(), &sale), Decision::Accept { quantity: 300.0 });
}

#[test]
fn cash_promised_to_a_short_put_cannot_buy_anything_else() {
    let held = BTreeMap::from([(
        "SPY250912P00640000.AOPT".to_owned(),
        Position { quantity: -100.0, entry: 2.0 },
    )]);
    let greedy = Proposal {
        desired_quantity: Some(1_000_000.0),
        ..proposal("MSFT.RH")
    };
    // $100k spendable, $64k of it reserved: 360 shares at $100, not 1,000.
    assert_eq!(decided(held, &greedy), Decision::Accept { quantity: 360.0 });
}

#[test]
fn a_cash_account_does_not_short_stock_or_sell_an_uncovered_call() {
    let stock = Proposal { opens_short: true, ..proposal("MSFT.RH") };
    assert!(matches!(
        decided(BTreeMap::new(), &stock),
        Decision::Reject(Rejection::CannotShort { .. })
    ));
    let call = Proposal {
        reference_price: 2.0,
        opens_short: true,
        ..proposal("SPY250912C00700000.AOPT")
    };
    assert!(matches!(
        decided(BTreeMap::new(), &call),
        Decision::Reject(Rejection::Uncovered { .. })
    ));
}

#[test]
fn an_option_is_sized_in_whole_contracts_and_pays_its_spread() {
    let costs = CostModel {
        option_spread: Some(crate::OptionSpread::MEASURED),
        ..CostModel::proportional(0.0, 0.0)
    };
    // $10k at $2.00 a share is 5,000 shares on price alone. Half the spread
    // is 2% of $2.00 = $0.04, so $2.04 a share all in: 4,901 shares, which
    // is 49 whole contracts and not 4,901 shares of a contract.
    assert_eq!(
        sized_option(2.0, 1e9, Some(&costs)),
        Decision::Accept { quantity: 4_900.0 }
    );
    // Asking for 250 shares is asking for two and a half contracts.
    assert_eq!(
        sized_option(2.0, 250.0, Some(&costs)),
        Decision::Accept { quantity: 200.0 }
    );
    // Less than one contract is nothing, not a fraction of one.
    assert!(matches!(
        sized_option(2.0, 99.0, Some(&costs)),
        Decision::Reject(Rejection::TooSmall { .. })
    ));
}

#[test]
fn a_stock_is_still_sized_in_shares() {
    assert!((sized(None, None) - 1_000.0).abs() < 1e-9);
    let costs = CostModel {
        option_spread: Some(crate::OptionSpread::MEASURED),
        ..CostModel::proportional(0.0, 0.0)
    };
    assert!(
        (sized(None, Some(&costs)) - 1_000.0).abs() < 1e-9,
        "an option spread on the record does not charge a stock"
    );
}

#[test]
fn an_account_that_has_spent_money_is_not_sized_as_if_it_had_not() {
    // Sizing comes off the opening balance on purpose. Without a cash
    // ceiling an account down to $50k kept ordering $100k of stock, the
    // venue refused every one, and a rule took 3 of its 72 signals.
    let costs = CostModel::proportional(1.0, 1.0);
    let quantity = sized(Some(50_000.0), Some(&costs));
    assert!((quantity - 499.0).abs() < 1e-9, "{quantity}");
    assert!(
        sized(Some(500_000.0), Some(&costs)) <= sized(None, Some(&costs)),
        "cash beyond the cap buys nothing past the cap"
    );
}

#[test]
fn a_desired_size_is_honoured_up_to_the_cap_and_no_further() {
    // The backtest engine trades a fixed quantity when no risk sizing
    // applies. Without this the same rule would trade 100 shares in a
    // backtest and the whole account live, which is the divergence the
    // shared policy exists to prevent.
    let gate = gate(model());
    let modest = Proposal {
        desired_quantity: Some(5.0),
        ..proposal("MSFT.RH")
    };
    assert_eq!(
        gate.propose(&modest, immediately(&modest), None),
        Decision::Accept { quantity: 5.0 }
    );

    let greedy = Proposal {
        desired_quantity: Some(1_000.0),
        ..proposal("MSFT.RH")
    };
    let Decision::Accept { quantity } = gate.propose(&greedy, immediately(&greedy), None) else {
        panic!("it is capped, not refused");
    };
    assert!((quantity - 20.0).abs() < 1e-9, "the cap bounds the request");
}

#[test]
fn an_account_too_small_for_one_unit_is_told_so() {
    // Named rather than returned as a zero quantity, which every caller
    // would have to remember to check and one of them would not.
    let gate = RiskGate::new(model(), 2_000.0, day(9));
    let expensive = Proposal {
        reference_price: 5_000.0,
        ..proposal("BRK-A.RH")
    };
    assert!(matches!(
        gate.propose(&expensive, immediately(&expensive), None),
        Decision::Reject(Rejection::TooSmall { .. })
    ));
}

#[test]
fn the_gate_books_fills_rather_than_its_own_acceptances() {
    // A proposal that is accepted may still not fill. A gate that assumed
    // otherwise would refuse trades on exposure the account does not have.
    let gate = gate(RiskModel {
        max_concurrent_positions: Some(1),
        ..model()
    });
    let first = proposal("MSFT.RH");
    assert!(matches!(
        gate.propose(&first, immediately(&first), None),
        Decision::Accept { .. }
    ));

    // Nothing filled, so nothing is held, so a second name still passes.
    let second = proposal("AAPL.RH");
    assert!(matches!(
        gate.propose(&second, immediately(&second), None),
        Decision::Accept { .. }
    ));
    assert!(gate.positions().is_empty());
}


/// A gate on a small margin account subject to the rule.
fn pdt_gate(starting: f64) -> RiskGate {
    RiskGate::new(
        RiskModel {
            day_trading: DayTradingRule::PatternDayTrader,
            ..model()
        },
        starting,
        day(9),
    )
}

/// Opens and closes in one day, which is what the rule counts.
fn day_trade(gate: &mut RiskGate, instrument: &str, on: NaiveDate) {
    gate.opened(instrument, 1.0, 100.0, on);
    gate.closed(instrument, 0.0, on);
}

#[test]
fn a_small_margin_account_gets_three_round_trips_and_then_stops() {
    // The constraint that decides what a two-thousand-dollar day-trading
    // system can attempt at all. A backtest that ignores it is backtesting
    // an account nobody can open.
    let mut gate = pdt_gate(2_000.0);
    for (index, instrument) in ["A.RH", "B.RH", "C.RH"].iter().enumerate() {
        let next = proposal(instrument);
        assert!(
            matches!(
                gate.propose(&next, immediately(&next), None),
                Decision::Accept { .. }
            ),
            "round trip {} of three should pass",
            index + 1
        );
        day_trade(&mut gate, instrument, day(9));
    }

    let fourth = proposal("D.RH");
    let Decision::Reject(Rejection::PatternDayTrader { used, limit, .. }) =
        gate.propose(&fourth, immediately(&fourth), None)
    else {
        panic!("the fourth would flag the account");
    };
    assert_eq!((used, limit), (3, PDT_DAY_TRADES));
}

#[test]
fn the_rule_does_not_apply_above_the_equity_floor() {
    // Twenty-five thousand is the line. Above it the account may day trade
    // freely, which is the whole point of the floor.
    let mut gate = pdt_gate(PDT_EQUITY_FLOOR + 1_000.0);
    for instrument in ["A.RH", "B.RH", "C.RH", "D.RH"] {
        day_trade(&mut gate, instrument, day(9));
    }
    let next = proposal("E.RH");
    assert!(matches!(
        gate.propose(&next, immediately(&next), None),
        Decision::Accept { .. }
    ));
}

#[test]
fn an_account_that_falls_below_the_floor_starts_being_constrained() {
    // The rule tests *current* equity, not the balance you opened with —
    // which is why AccountState carries both.
    let mut gate = pdt_gate(PDT_EQUITY_FLOOR + 1_000.0);
    for instrument in ["A.RH", "B.RH", "C.RH"] {
        day_trade(&mut gate, instrument, day(9));
    }
    gate.mark(PDT_EQUITY_FLOOR - 1.0);

    let next = proposal("D.RH");
    assert!(matches!(
        gate.propose(&next, immediately(&next), None),
        Decision::Reject(Rejection::PatternDayTrader { .. })
    ));
}

#[test]
fn a_position_held_overnight_is_not_a_day_trade() {
    // Only a round trip opened and closed on one day counts. Counting a
    // held position would exhaust the budget on a rule that never day
    // trades at all.
    let mut gate = pdt_gate(2_000.0);
    for (index, instrument) in ["A.RH", "B.RH", "C.RH"].iter().enumerate() {
        gate.opened(instrument, 1.0, 100.0, day(9));
        gate.closed(instrument, 0.0, day(10 + u32::try_from(index).expect("small")));
    }
    let next = proposal("D.RH");
    assert!(
        matches!(
            gate.propose(&next, immediately(&next), None),
            Decision::Accept { .. }
        ),
        "three overnight round trips use none of the budget"
    );
}

#[test]
fn the_budget_rolls_off_after_five_business_days() {
    let mut gate = pdt_gate(2_000.0);
    // Three day trades on the 1st of September 2026, a Tuesday.
    let long_ago = NaiveDate::from_ymd_opt(2026, 9, 1).expect("valid");
    for instrument in ["A.RH", "B.RH", "C.RH"] {
        day_trade(&mut gate, instrument, long_ago);
    }
    assert_eq!(gate.day_trades_used(long_ago), 3);
    // Two weeks later they are outside any five-business-day window.
    assert_eq!(
        gate.day_trades_used(NaiveDate::from_ymd_opt(2026, 9, 15).expect("valid")),
        0
    );
}

#[test]
fn an_unconstrained_account_is_not_asked_about_day_trades() {
    // Every finding recorded before this existed ran unconstrained, and
    // switching the rule on silently would have changed all of them.
    assert_eq!(RiskModel::default().day_trading, DayTradingRule::Unconstrained);
    let mut gate = gate(model());
    for instrument in ["A.RH", "B.RH", "C.RH", "D.RH", "E.RH"] {
        day_trade(&mut gate, instrument, day(9));
    }
    let next = proposal("F.RH");
    assert!(matches!(
        gate.propose(&next, immediately(&next), None),
        Decision::Accept { .. }
    ));
}

#[test]
fn the_rule_refuses_an_entry_and_never_an_exit() {
    // The design constraint this whole shape follows from: the trade that
    // flags an account is the *closing* one, and an exit may never be
    // refused. So the constraint has to bite at the entry instead.
    let mut gate = pdt_gate(2_000.0);
    for instrument in ["A.RH", "B.RH", "C.RH"] {
        day_trade(&mut gate, instrument, day(9));
    }
    gate.opened("HELD.RH", 5.0, 100.0, day(9));

    let next = proposal("D.RH");
    assert!(matches!(
        gate.propose(&next, immediately(&next), None),
        Decision::Reject(Rejection::PatternDayTrader { .. })
    ));
    // Closing is not a proposal and never passes through the gate's
    // refusals — the position can still be shed.
    gate.closed("HELD.RH", -50.0, day(9));
    assert!(gate.positions().is_empty(), "the exit is always available");
}

#[test]
fn day_trades_are_counted_the_same_way_from_a_ledger() {
    // A live session counts from its own book and a backtest counts from
    // the engine's ledger. Counting differently would be two systems, and
    // the stored finding would describe neither.
    // `day` is a date; a trade is stamped with an instant.
    let opened = day(9).and_time(chrono::NaiveTime::MIN);
    let same_day = Trade {
        instrument: "A.RH".to_owned(),
        opened,
        closed: Some(opened),
        direction: crate::Direction::Long,
        quantity: 1.0,
        entry: 100.0,
        exit: Some(101.0),
        pnl: 1.0,
        commission: 0.0,
        exit_reason: crate::ExitReason::Signal,
    };
    let overnight = Trade {
        closed: Some(day(10).and_time(chrono::NaiveTime::MIN)),
        ..same_day.clone()
    };
    let still_open = Trade {
        closed: None,
        ..same_day.clone()
    };

    let ledger = vec![same_day, overnight, still_open];
    assert_eq!(
        day_trades_in_window(&ledger, day(9), PDT_WINDOW_DAYS),
        1,
        "only the round trip that opened and closed on one day counts"
    );
}

#[test]
fn a_limit_that_can_never_bind_or_always_binds_is_refused() {
    // Every other field in the model is validated; these two were not, so a
    // daily limit of 150% or a correlation cap of zero would have been
    // accepted and then quietly done nothing, or refused everything.
    let bad_daily = RiskModel {
        max_daily_loss: Some(1.5),
        ..RiskModel::default()
    };
    assert!(bad_daily.check().is_err());

    let zero_cap = RiskModel {
        correlation_cap: Some(CorrelationCap {
            above: 0.8,
            max_positions: 0,
        }),
        ..RiskModel::default()
    };
    assert!(
        zero_cap.check().is_err(),
        "a cap of zero refuses every correlated trade; remove the cap instead"
    );

    let impossible_rho = RiskModel {
        correlation_cap: Some(CorrelationCap {
            above: 1.7,
            max_positions: 1,
        }),
        ..RiskModel::default()
    };
    assert!(impossible_rho.check().is_err(), "no pair correlates above 1");
}

#[test]
fn a_workable_limit_passes_validation() {
    let model = RiskModel {
        max_daily_loss: Some(0.02),
        correlation_cap: Some(CorrelationCap {
            above: 0.8,
            max_positions: 1,
        }),
        ..RiskModel::default()
    };
    assert!(model.check().is_ok());
}
