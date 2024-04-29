//! An example of a function that will hang when mutated.
//!
//! An attribute could be added to avoid mutating it, but this tree
//! lets us test the case where that has not yet been fixed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static TRIGGER: AtomicBool = AtomicBool::new(false);

const fn should_stop_const() -> bool {
    true
}

/// If `should_stop_const` is mutated to return false, then this const block
/// will hang and block compilation.
pub const VAL: i32 = loop {
    if should_stop_const() {
        break 1;
    }
};

/// If mutated to return false, the program will spin forever.
fn should_stop() -> bool {
    if TRIGGER.load(Ordering::Relaxed) {
        return true;
    }
    TRIGGER.store(true, Ordering::Relaxed);
    false
}

/// Runs until `should_stop` returns true, and then returns the number
/// of iterations.
///
/// Also stops after a few minutes anyhow, so that if the timeouts are not
/// properly implemented, the child process doesn't hang around forever.
pub fn controlled_loop() -> usize {
    let start = Instant::now();
    for i in 1.. {
        println!("{}", i);
        if should_stop() {
            return i;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        if start.elapsed() > Duration::from_secs(60 * 5) {
            panic!("timed out");
        }
    }
    unreachable!();
}

mod test {
    #[test]
    fn controlled_loop_terminates() {
        // Should do two passes: first the trigger is false but gets set,
        // then the trigger is true and the loop terminates.
        assert_eq!(super::controlled_loop(), 2);
    }

    #[test]
    fn val_is_correct() {
        assert_eq!(super::VAL, 1);
    }
}
