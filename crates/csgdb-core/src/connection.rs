/// The current transaction activity for a database connection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(i32)]
pub enum TransactionState {
    /// No transaction currently accesses the selected database.
    None = 0,
    /// A transaction has read from the selected database.
    Read = 1,
    /// A transaction has written to the selected database.
    Write = 2,
}
