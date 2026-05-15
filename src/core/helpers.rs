use super::*;

#[inline]
pub(crate) fn std_read_guard<'a, T>(
    lock: &'a StdRwLock<T>,
    context: &'static str,
) -> StdReadGuard<'a, T> {
    match lock.read() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!(context = %context, "std rwlock poisoned (read)");
            poisoned.into_inner()
        }
    }
}

#[inline]
pub(crate) fn std_write_guard<'a, T>(
    lock: &'a StdRwLock<T>,
    context: &'static str,
) -> StdWriteGuard<'a, T> {
    match lock.write() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::error!(context = %context, "std rwlock poisoned (write)");
            poisoned.into_inner()
        }
    }
}
