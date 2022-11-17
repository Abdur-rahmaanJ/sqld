use std::fmt;

use anyhow::Result;
use sqlparser::{ast::Statement, dialect::SQLiteDialect, parser::Parser};

/// A group of statements to be executed together.
pub struct Statements {
    pub stmts: String,
    kinds: Vec<StmtKind>,
}

impl fmt::Debug for Statements {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.stmts)
    }
}

/// Classify statement in categories of interest.
#[derive(Debug, PartialEq, Clone, Copy)]
enum StmtKind {
    /// The begining of a transaction
    TxnBegin,
    /// The end of a transaction
    TxnEnd,
    Other,
}

impl StmtKind {
    fn kind(stmt: &Statement) -> Self {
        match stmt {
            Statement::StartTransaction { .. } => Self::TxnBegin,
            Statement::SetTransaction { .. } => todo!("handle set txn"),
            Statement::Rollback { .. } | Statement::Commit { .. } => Self::TxnEnd,
            Statement::Savepoint { .. } => todo!("handle savepoint"),
            // FIXME: this contains lots of dialect specific nodes, when porting to Postges, check what's
            // in there.
            _ => Self::Other,
        }
    }
}

/// The state of a transaction for a series of statement
pub enum State {
    /// The txn in a opened state
    TxnOpened,
    /// The txn in a closed state
    TxnClosed,
    /// This is the initial state of the state machine
    Start,
    /// This is an invalid state for the state machine
    Invalid,
}

impl Statements {
    pub fn parse(s: String) -> Result<Self> {
        let statements = Parser::parse_sql(&SQLiteDialect {}, &s)?;
        // We don't actually really care about `StmtKind::Other`, we keep keep it for conceptual simplicity.
        let kinds = statements.iter().map(StmtKind::kind).collect();

        Ok(Self { stmts: s, kinds })
    }

    /// Given an initial state, returns final state a transaction should be in after running these
    /// statements.
    pub fn state(&self, state: State) -> State {
        self.kinds
            .iter()
            .fold(state, |old_state, current| match (old_state, current) {
                (State::TxnOpened, StmtKind::TxnBegin) | (State::TxnClosed, StmtKind::TxnEnd) => {
                    State::Invalid
                }
                (State::TxnOpened, StmtKind::TxnEnd) => State::TxnClosed,
                (State::TxnClosed, StmtKind::TxnBegin) => State::TxnOpened,
                (state, StmtKind::Other) => state,
                (State::Invalid, _) => State::Invalid,
                (State::Start, StmtKind::TxnBegin) => State::TxnOpened,
                (State::Start, StmtKind::TxnEnd) => State::TxnClosed,
            })
    }
}
