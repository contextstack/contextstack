use super::sql::tokens;
use rusqlite::{
    CachedStatement, Connection as RawConnection, Error, MappedRows, Result, Row, Statement, ToSql,
    Transaction as RawTransaction, TransactionBehavior,
};
use std::ops::Deref;

#[derive(Clone, Debug)]
pub struct OrgScope {
    pub org_id: String,
    pub member_id: String,
    /// Internal store operations carry a bookkeeping member without a human role.
    pub role: Option<crate::access::Role>,
}

/// Runtime access to the store carries the validated request identity, including inside writes.
/// Use `+org_id = :org_id` in filters so SQLite does not choose an org-only index
/// ahead of a selective lookup.
pub struct ScopedConnection {
    raw: RawConnection,
    scope: OrgScope,
}

impl ScopedConnection {
    pub(super) fn new(raw: RawConnection, scope: OrgScope) -> Self {
        Self { raw, scope }
    }

    pub fn scope(&self) -> &OrgScope {
        &self.scope
    }

    #[cfg(test)]
    pub fn raw(&self) -> &RawConnection {
        &self.raw
    }

    #[cfg(test)]
    pub fn fixture_batch(&self, sql: &str) -> Result<()> {
        for statement in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            self.execute(statement, rusqlite::params![])?;
        }
        Ok(())
    }

    pub fn last_insert_rowid(&self) -> i64 {
        self.raw.last_insert_rowid()
    }

    pub fn savepoint_sql(&self, sql: &str) -> Result<()> {
        self.raw.execute_batch(sql)
    }

    pub fn unchecked_transaction(&self) -> Result<ScopedTransaction<'_>> {
        if crate::policy::has_write_request() {
            return immediate(self);
        }
        let raw = self.raw.unchecked_transaction()?;
        Ok(ScopedTransaction { raw, db: self })
    }

    pub fn prepare(&self, sql: &str) -> Result<ScopedStatement<'_>> {
        let (sql, identity) = scope_parameters(sql)?;
        Ok(ScopedStatement {
            statement: Prepared::Owned(self.raw.prepare(&sql)?),
            scope: &self.scope,
            identity,
        })
    }

    pub fn prepare_cached(&self, sql: &str) -> Result<ScopedStatement<'_>> {
        let (sql, identity) = scope_parameters(sql)?;
        Ok(ScopedStatement {
            statement: Prepared::Cached(self.raw.prepare_cached(&sql)?),
            scope: &self.scope,
            identity,
        })
    }

    pub fn execute<I: IntoIterator<Item: ToSql>>(&self, sql: &str, args: I) -> Result<usize> {
        self.prepare(sql)?.execute(args)
    }

    pub fn query_row<T, I, F>(&self, sql: &str, args: I, read: F) -> Result<T>
    where
        I: IntoIterator<Item: ToSql>,
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        self.prepare(sql)?.query_row(args, read)
    }
}

pub struct ScopedTransaction<'a> {
    raw: RawTransaction<'a>,
    db: &'a ScopedConnection,
}

pub fn write_tx(db: &mut ScopedConnection) -> Result<ScopedTransaction<'_>> {
    immediate(db)
}

fn immediate(db: &ScopedConnection) -> Result<ScopedTransaction<'_>> {
    let raw = RawTransaction::new_unchecked(&db.raw, TransactionBehavior::Immediate)?;
    crate::policy::recheck_write(db)?;
    Ok(ScopedTransaction { raw, db })
}

impl Deref for ScopedTransaction<'_> {
    type Target = ScopedConnection;

    fn deref(&self) -> &ScopedConnection {
        self.db
    }
}

impl ScopedTransaction<'_> {
    pub fn commit(self) -> Result<()> {
        self.raw.commit()
    }
}

#[derive(Clone, Copy, Default)]
struct IdentityParameters {
    org: bool,
    member: bool,
}

impl IdentityParameters {
    fn count(self) -> usize {
        usize::from(self.org) + usize::from(self.member)
    }
}

/// Reserve leading slots for identity without changing the numbering of caller parameters.
/// Quoted SQL strings and identifiers can contain literal question marks.
fn scope_parameters(sql: &str) -> Result<(std::borrow::Cow<'_, str>, IdentityParameters)> {
    let mut identity = IdentityParameters::default();
    if sql.contains(":org_id") || sql.contains(":member_id") {
        for (_, token) in tokens(sql) {
            identity.org |= token == ":org_id";
            identity.member |= token == ":member_id";
        }
    }
    let offset = identity.count();
    if offset == 0 {
        return Ok((sql.into(), identity));
    }

    let mut output = String::with_capacity(sql.len());
    let mut cursor = 0;
    let mut largest = 0;
    for (span, token) in tokens(sql) {
        let slot = match token {
            ":org_id" => 1,
            ":member_id" => offset,
            token if token.starts_with('?') => {
                let invalid = || Error::InvalidParameterName(token.to_owned());
                let index = if token == "?" {
                    largest + 1
                } else {
                    token[1..].parse::<usize>().map_err(|_| invalid())?
                };
                largest = largest.max(index);
                index.checked_add(offset).ok_or_else(invalid)?
            }
            _ => continue,
        };
        output.push_str(&sql[cursor..span.start]);
        output.push('?');
        output.push_str(&slot.to_string());
        cursor = span.end;
    }
    output.push_str(&sql[cursor..]);
    Ok((output.into(), identity))
}

enum Prepared<'a> {
    Owned(Statement<'a>),
    Cached(CachedStatement<'a>),
}

pub struct ScopedStatement<'a> {
    statement: Prepared<'a>,
    scope: &'a OrgScope,
    identity: IdentityParameters,
}

impl<'a> ScopedStatement<'a> {
    fn bind(&mut self, args: impl IntoIterator<Item: ToSql>) -> Result<&mut Statement<'a>> {
        let stmt = match &mut self.statement {
            Prepared::Owned(stmt) => stmt,
            Prepared::Cached(stmt) => &mut **stmt,
        };
        let offset = self.identity.count();
        if self.identity.org {
            stmt.raw_bind_parameter(1, &self.scope.org_id)?;
        }
        if self.identity.member {
            stmt.raw_bind_parameter(offset, &self.scope.member_id)?;
        }
        let expected = stmt.parameter_count() - offset;
        let mut count = 0;
        for arg in args {
            count += 1;
            if count > expected {
                return Err(Error::InvalidParameterCount(count, expected));
            }
            stmt.raw_bind_parameter(count + offset, arg)?;
        }
        if count != expected {
            return Err(Error::InvalidParameterCount(count, expected));
        }
        Ok(stmt)
    }

    pub fn execute(&mut self, args: impl IntoIterator<Item: ToSql>) -> Result<usize> {
        self.bind(args)?.raw_execute()
    }

    pub fn exists(&mut self, args: impl IntoIterator<Item: ToSql>) -> Result<bool> {
        Ok(self.bind(args)?.raw_query().next()?.is_some())
    }

    pub fn query_row<T>(
        &mut self,
        args: impl IntoIterator<Item: ToSql>,
        read: impl FnOnce(&Row<'_>) -> Result<T>,
    ) -> Result<T> {
        read(
            self.bind(args)?
                .raw_query()
                .next()?
                .ok_or(Error::QueryReturnedNoRows)?,
        )
    }

    pub fn query_map<T, F>(
        &mut self,
        args: impl IntoIterator<Item: ToSql>,
        read: F,
    ) -> Result<MappedRows<'_, F>>
    where
        F: FnMut(&Row<'_>) -> Result<T>,
    {
        Ok(self.bind(args)?.raw_query().mapped(read))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_repeated_identity_and_mixed_positional_parameters() -> Result<()> {
        let db = ScopedConnection::new(
            RawConnection::open_in_memory()?,
            OrgScope {
                org_id: "org".into(),
                member_id: "member".into(),
                role: None,
            },
        );
        let values: String = db.query_row(
            "SELECT ?2 || :org_id || ?1 || ? || :member_id || :org_id || '?1:org_id' /* ?9 */",
            ["first", "second", "third"],
            |row| row.get(0),
        )?;
        assert_eq!(values, "secondorgfirstthirdmemberorg?1:org_id");
        Ok(())
    }
}
