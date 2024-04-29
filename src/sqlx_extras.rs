//! Helpers for sqlx queries.

pub (crate) trait Extras<'q, DB: sqlx::Database> {

    /// Add an in clause.
    /// Collapses the case where the  iterator is empty to "false" instead of invalid SQL.
    fn is_in<SqlItem>(&mut self, expression: impl AsRef<str>, items: impl IntoIterator<Item = SqlItem>)
    where SqlItem: 'q + sqlx::Encode<'q, DB> + Send + sqlx::Type<DB>;
}

impl <'q, DB: sqlx::Database> Extras<'q, DB> for sqlx::QueryBuilder<'q, DB>
{
    fn is_in<SqlItem>(&mut self, expression: impl AsRef<str>, items: impl IntoIterator<Item = SqlItem>)
    where SqlItem: 'q + sqlx::Encode<'q, DB> + Send + sqlx::Type<DB> {
        let mut iter = items.into_iter();
        let Some(first) = iter.next() else {
            self.push("FALSE");
            return;
        };

        self.push(expression.as_ref());
        self.push(" IN (");
        let mut joiner = self.separated(", ");
        joiner.push_bind(first);
        for rest in iter {
            joiner.push_bind(rest);
        }
        self.push(")");
    }
}
