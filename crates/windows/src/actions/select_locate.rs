//! Locating a named row across the scroll windows a virtualized container renders.
//!
//! Proving a name unique means scanning every window to the end of the scroll
//! range, and a row is unrealized as the sweep passes it. The window a row was
//! sighted in is recorded, so the search returns there to read the row again
//! instead of reporting a row it has already seen as absent.

use agent_desktop_core::AdapterError;

pub(crate) const MAX_REALIZE_SCROLLS: u32 = 16;

/// One realize scroll attempt: either expose a searchable window or stop.
pub(crate) enum RealizeScrollStep {
    Window { end_of_range: bool },
    Stop,
}

/// What a located search asks of the container it searches.
pub(crate) trait RealizeScan {
    type Item: Clone;

    /// Scans the rows the container has built; a match that is not `prior` is a second row.
    fn scan(&mut self, prior: Option<&Self::Item>) -> Result<Option<Self::Item>, AdapterError>;
    /// Whether a vertical scroll can hide rows from `scan`.
    fn hides_rows_behind_a_scroll(&self) -> bool;
    /// The scroll position of the window `scan` reads, when the container reports one.
    fn position(&self) -> Option<f64>;
    /// Scrolls one step toward the end of the range.
    fn scroll_step(&mut self) -> Result<RealizeScrollStep, AdapterError>;
    /// Scrolls back to `position`; `false` when the container cannot.
    fn scroll_to(&mut self, position: f64) -> Result<bool, AdapterError>;
}

/// The answer of the search.
pub(crate) enum Located<T> {
    Found(T),
    Absent,
    /// Nothing matched and the scroll that could show more rows is not allowed.
    ScrollDenied,
}

/// A matching row and the scroll position of the window it was sighted in.
struct Sighting<T> {
    item: T,
    position: Option<f64>,
}

enum Phase<T> {
    FirstScan,
    Sweep(Option<Sighting<T>>),
    Return(Sighting<T>),
    Done(Located<T>),
}

/// Finds the named row. `may_scroll` is whether the policy allows calls that change what the user sees.
pub(crate) fn locate<S: RealizeScan>(
    source: &mut S,
    may_scroll: bool,
) -> Result<Located<S::Item>, AdapterError> {
    let mut phase = Phase::FirstScan;
    loop {
        phase = match phase {
            Phase::FirstScan => first_scan(source, may_scroll)?,
            Phase::Sweep(sighting) => sweep(source, sighting)?,
            Phase::Return(sighting) => return_to(source, sighting)?,
            Phase::Done(located) => return Ok(located),
        };
    }
}

fn first_scan<S: RealizeScan>(
    source: &mut S,
    may_scroll: bool,
) -> Result<Phase<S::Item>, AdapterError> {
    let position = source.position();
    let scanned = source.scan(None)?;
    if may_scroll {
        return Ok(Phase::Sweep(
            scanned.map(|item| Sighting { item, position }),
        ));
    }
    Ok(match scanned {
        Some(item) => Phase::Done(Located::Found(item)),
        None if source.hides_rows_behind_a_scroll() => Phase::Done(Located::ScrollDenied),
        None => Phase::Done(Located::Absent),
    })
}

/// Scrolls to the end of the range, scanning every window so a second row is
/// `AMBIGUOUS_TARGET`. `realized` is the row the most recent scan found, which
/// is the one to select while the container stays on that window; a sighting
/// the sweep scrolled away from is returned to before the search gives up.
fn sweep<S: RealizeScan>(
    source: &mut S,
    mut sighting: Option<Sighting<S::Item>>,
) -> Result<Phase<S::Item>, AdapterError> {
    let mut realized = sighting.as_ref().map(|seen| seen.item.clone());
    for _ in 0..MAX_REALIZE_SCROLLS {
        let end_of_range = match source.scroll_step()? {
            RealizeScrollStep::Stop => break,
            RealizeScrollStep::Window { end_of_range } => end_of_range,
        };
        let position = source.position();
        let scanned = {
            let prior = sighting.as_ref().map(|seen| &seen.item);
            source.scan(prior)?
        };
        if sighting.is_none() {
            if let Some(item) = &scanned {
                sighting = Some(Sighting {
                    item: item.clone(),
                    position,
                });
            }
        }
        realized = scanned;
        if end_of_range {
            break;
        }
    }
    Ok(match (sighting, realized) {
        (Some(_), Some(item)) => Phase::Done(Located::Found(item)),
        (Some(seen), None) => Phase::Return(seen),
        (None, _) => Phase::Done(Located::Absent),
    })
}

fn return_to<S: RealizeScan>(
    source: &mut S,
    sighting: Sighting<S::Item>,
) -> Result<Phase<S::Item>, AdapterError> {
    let Some(position) = sighting.position else {
        return Ok(Phase::Done(Located::Absent));
    };
    if !source.scroll_to(position)? {
        return Ok(Phase::Done(Located::Absent));
    }
    Ok(match source.scan(Some(&sighting.item))? {
        Some(item) => Phase::Done(Located::Found(item)),
        None => Phase::Done(Located::Absent),
    })
}

#[cfg(test)]
#[path = "select_locate_tests.rs"]
mod tests;
