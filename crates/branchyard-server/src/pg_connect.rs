//! Opening the server's PostgreSQL connections, shared by its stores.

use std::io;

/// Make `conn` an open connection to `url`, replacing a missing or closed
/// one. The connection is made on the calling thread, off any Tokio
/// runtime (`branchyard::postgres_open` takes care of that); a closed
/// client is dropped on a thread of its own, since closing blocks.
pub(crate) fn reconnect(
    conn: &mut Option<postgres::Client>,
    url: &str,
    what: &str,
) -> io::Result<()> {
    if conn.as_ref().is_some_and(|c| !c.is_closed()) {
        return Ok(());
    }
    let old = conn.take();
    let fresh = branchyard::postgres_open(url, None)
        .map_err(|e| io::Error::other(format!("{what}: {e}")))?;
    *conn = Some(fresh);
    if let Some(old) = old {
        std::thread::scope(|scope| {
            scope.spawn(move || drop(old));
        });
    }
    Ok(())
}
