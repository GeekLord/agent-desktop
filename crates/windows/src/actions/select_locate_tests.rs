use super::{Located, MAX_REALIZE_SCROLLS, RealizeScan, RealizeScrollStep, locate};
use agent_desktop_core::{AdapterError, ErrorCode};

struct ScrollShape {
    scrollable: bool,
    reports_position: bool,
    can_return: bool,
}

struct Fake {
    windows: Vec<Vec<u8>>,
    named: Vec<u8>,
    at: usize,
    shape: ScrollShape,
    log: Vec<String>,
}

impl Fake {
    fn new(windows: Vec<Vec<u8>>, named: Vec<u8>) -> Self {
        Self {
            windows,
            named,
            at: 0,
            shape: ScrollShape {
                scrollable: true,
                reports_position: true,
                can_return: true,
            },
            log: Vec::new(),
        }
    }

    fn scans(&self) -> Vec<&str> {
        self.log
            .iter()
            .map(String::as_str)
            .filter(|entry| entry.starts_with("scan"))
            .collect()
    }

    fn returns(&self) -> Vec<&str> {
        self.log
            .iter()
            .map(String::as_str)
            .filter(|entry| entry.starts_with("return"))
            .collect()
    }
}

impl RealizeScan for Fake {
    type Item = u8;

    fn scan(&mut self, prior: Option<&u8>) -> Result<Option<u8>, AdapterError> {
        self.log.push(format!("scan@{}", self.at));
        let window = &self.windows[self.at];
        let matches: Vec<u8> = window
            .iter()
            .copied()
            .filter(|id| self.named.contains(id))
            .collect();
        let Some(&first) = matches.first() else {
            return Ok(None);
        };
        let differs = |id: &u8| *id != first;
        if matches.iter().any(differs) || prior.is_some_and(|known| *known != first) {
            return Err(AdapterError::ambiguous_target("two rows share the name"));
        }
        Ok(Some(first))
    }

    fn hides_rows_behind_a_scroll(&self) -> bool {
        self.shape.scrollable
    }

    fn position(&self) -> Option<f64> {
        self.shape.reports_position.then_some(self.at as f64)
    }

    fn scroll_step(&mut self) -> Result<RealizeScrollStep, AdapterError> {
        if !self.shape.scrollable {
            return Ok(RealizeScrollStep::Stop);
        }
        let end_of_range = self.at + 1 >= self.windows.len();
        if !end_of_range {
            self.at += 1;
        }
        Ok(RealizeScrollStep::Window { end_of_range })
    }

    fn scroll_to(&mut self, position: f64) -> Result<bool, AdapterError> {
        self.log.push(format!("return@{position}"));
        if !self.shape.can_return {
            return Ok(false);
        }
        self.at = position as usize;
        Ok(true)
    }
}

fn found(located: Located<u8>) -> Option<u8> {
    match located {
        Located::Found(item) => Some(item),
        Located::Absent | Located::ScrollDenied => None,
    }
}

#[test]
fn a_row_sighted_in_an_intermediate_window_is_returned_to_after_the_sweep() {
    let mut fake = Fake::new(vec![vec![1], vec![2], vec![3], vec![4]], vec![2]);
    let located = locate(&mut fake, true).expect("locate");
    assert_eq!(found(located), Some(2));
    assert_eq!(fake.returns(), vec!["return@1"]);
    assert_eq!(fake.at, 1);
}

#[test]
fn a_row_the_final_window_realizes_is_found_without_scrolling_back() {
    let mut fake = Fake::new(vec![vec![1, 2], vec![2, 3]], vec![2]);
    let located = locate(&mut fake, true).expect("locate");
    assert_eq!(found(located), Some(2));
    assert_eq!(fake.returns(), Vec::<&str>::new());
}

#[test]
fn an_absent_name_scans_every_window_then_reports_absence() {
    let mut fake = Fake::new(vec![vec![1], vec![2], vec![3]], vec![9]);
    let located = locate(&mut fake, true).expect("locate");
    assert!(matches!(located, Located::Absent));
    assert_eq!(fake.scans(), vec!["scan@0", "scan@1", "scan@2", "scan@2"]);
    assert_eq!(fake.returns(), Vec::<&str>::new());
}

#[test]
fn a_sighting_without_a_reported_position_is_absent_rather_than_stale() {
    let mut fake = Fake::new(vec![vec![1], vec![2], vec![3]], vec![2]);
    fake.shape.reports_position = false;
    let located = locate(&mut fake, true).expect("locate");
    assert!(matches!(located, Located::Absent));
    assert_eq!(fake.returns(), Vec::<&str>::new());
}

#[test]
fn a_container_that_cannot_return_to_the_sighting_is_absent() {
    let mut fake = Fake::new(vec![vec![1], vec![2], vec![3]], vec![2]);
    fake.shape.can_return = false;
    let located = locate(&mut fake, true).expect("locate");
    assert!(matches!(located, Located::Absent));
    assert_eq!(fake.returns(), vec!["return@1"]);
}

#[test]
fn a_denied_scroll_never_touches_the_container() {
    let mut fake = Fake::new(vec![vec![1], vec![2]], vec![2]);
    let located = locate(&mut fake, false).expect("locate");
    assert!(matches!(located, Located::ScrollDenied));
    assert_eq!(fake.log, vec!["scan@0"]);
}

#[test]
fn a_denied_scroll_still_finds_a_row_the_current_window_realized() {
    let mut fake = Fake::new(vec![vec![1], vec![2]], vec![1]);
    let located = locate(&mut fake, false).expect("locate");
    assert_eq!(found(located), Some(1));
    assert_eq!(fake.log, vec!["scan@0"]);
}

#[test]
fn a_container_without_a_scroll_finds_its_realized_row() {
    let mut fake = Fake::new(vec![vec![1], vec![2]], vec![1]);
    fake.shape.scrollable = false;
    let located = locate(&mut fake, true).expect("locate");
    assert_eq!(found(located), Some(1));
    assert_eq!(fake.log, vec!["scan@0"]);
}

#[test]
fn a_second_row_in_a_later_window_is_ambiguous() {
    let mut fake = Fake::new(vec![vec![1], vec![1, 2]], vec![1, 2]);
    let located = locate(&mut fake, true);
    assert!(
        matches!(&located, Err(error) if error.code == ErrorCode::AmbiguousTarget),
        "a second row must refuse the select"
    );
}

#[test]
fn a_row_sighted_in_the_first_window_survives_the_sweep_to_the_end() {
    let windows = vec![vec![0], vec![1], vec![2], vec![3], vec![4], vec![5]];
    let mut fake = Fake::new(windows, vec![0]);
    let located = locate(&mut fake, true).expect("locate");
    assert_eq!(found(located), Some(0));
    assert_eq!(fake.returns(), vec!["return@0"]);
}

#[test]
fn the_sweep_stops_at_the_realize_scroll_limit() {
    let mut windows = vec![Vec::new(); MAX_REALIZE_SCROLLS as usize + 4];
    windows.push(vec![7]);
    let mut fake = Fake::new(windows, vec![7]);
    let located = locate(&mut fake, true).expect("locate");
    assert!(matches!(located, Located::Absent));
    assert_eq!(fake.at, MAX_REALIZE_SCROLLS as usize);
    assert_eq!(fake.returns(), Vec::<&str>::new());
}
