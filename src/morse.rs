//! Morse-code status indicator + message blinker — runs on core1.
//!
//! core1 keeps the latest network [`Status`] (received as [`Evt::Status`] from
//! core0) and continuously clocks out that status's Morse word on the onboard
//! LED. It also accepts free-text messages submitted on the captive portal
//! ([`MSG`]): when one arrives it blinks the message once, then resumes the
//! status word.
//!
//! core1 does not touch the LED directly — the LED is a CYW43 GPIO owned by
//! core0 — so it sends [`Cmd::SetLed`] commands and core0 performs the toggle.
//! The visible result: the LED spells the current network state (and any typed
//! message), with the *meaning* computed on core1 and the *action* on core0 —
//! a proof of the cross-core link both ways.
//!
//! Timing note: `embassy_time::Timer` does NOT work on core1 — the timer alarm
//! IRQ is unmasked only in core0's (per-core) NVIC by `embassy_rp::init`. We use
//! `embassy_time::block_for`, a busy-wait that polls the shared hardware timer
//! counter and needs no interrupt. Busy-waiting is fine: core1 is dedicated to
//! this one job.
//!
//! Status → word:
//!   Portal → "AP" (.- .--.), Scanning → "S" (...), Connecting → "C" (-.-.),
//!   Connected → "OK" (--- -.-), Failed → "ERR" (. .-. .-.).

use embassy_time::{block_for, Duration};

use crate::ipc::{Cmd, Evt, CMD, EVT, MSG};
use crate::shared::Status;

/// One Morse time unit. Dot = 1 unit; everything else is a multiple. 150 ms is
/// slow enough to read by eye.
const UNIT: Duration = Duration::from_millis(150);

/// Morse word for a network status (used as the idle/looping pattern).
fn word_for(status: Status) -> &'static str {
    match status {
        Status::Portal => ".- .--.",   // AP
        Status::Scanning => "...",      // S
        Status::Connecting => "-.-.",   // C
        Status::Connected => "--- -.-", // OK
        Status::Failed => ". .-. .-.",  // ERR
    }
}

/// Map one character to its Morse code (dots/dashes), or `None` if unsupported.
/// Letters are case-insensitive; space is handled by the caller as a word gap.
fn morse_for(c: char) -> Option<&'static str> {
    let code = match c.to_ascii_uppercase() {
        'A' => ".-",
        'B' => "-...",
        'C' => "-.-.",
        'D' => "-..",
        'E' => ".",
        'F' => "..-.",
        'G' => "--.",
        'H' => "....",
        'I' => "..",
        'J' => ".---",
        'K' => "-.-",
        'L' => ".-..",
        'M' => "--",
        'N' => "-.",
        'O' => "---",
        'P' => ".--.",
        'Q' => "--.-",
        'R' => ".-.",
        'S' => "...",
        'T' => "-",
        'U' => "..-",
        'V' => "...-",
        'W' => ".--",
        'X' => "-..-",
        'Y' => "-.--",
        'Z' => "--..",
        '0' => "-----",
        '1' => ".----",
        '2' => "..---",
        '3' => "...--",
        '4' => "....-",
        '5' => ".....",
        '6' => "-....",
        '7' => "--...",
        '8' => "---..",
        '9' => "----.",
        '.' => ".-.-.-",
        ',' => "--..--",
        '?' => "..--..",
        '\'' => ".----.",
        '!' => "-.-.--",
        '/' => "-..-.",
        '(' => "-.--.",
        ')' => "-.--.-",
        '&' => ".-...",
        ':' => "---...",
        ';' => "-.-.-.",
        '=' => "-...-",
        '+' => ".-.-.",
        '-' => "-....-",
        '_' => "..--.-",
        '"' => ".-..-.",
        '@' => ".--.-.",
        _ => return None,
    };
    Some(code)
}

/// Set the LED via core0 (non-blocking) and busy-wait `units` time units.
fn led_for(on: bool, units: u32) {
    let _ = CMD.try_send(Cmd::SetLed(on));
    block_for(UNIT * units);
}

/// Blink one already-encoded Morse letter (a string of '.'/'-'). Assumes the LED
/// is off on entry; leaves it off (the gap is the caller's responsibility).
fn blink_letter(code: &str) {
    for (i, sym) in code.chars().enumerate() {
        if i > 0 {
            led_for(false, 1); // intra-letter gap
        }
        match sym {
            '.' => led_for(true, 1),
            '-' => led_for(true, 3),
            _ => {}
        }
    }
}

/// Blink a pre-encoded status word (letters as '.'/'-', space-separated).
/// Returns early (aborts) if the network status changes mid-word.
fn blink_status_word(word: &str, current: Status) -> Option<Status> {
    let _ = CMD.try_send(Cmd::SetLed(false));
    let mut first = true;
    for letter in word.split(' ') {
        if !first {
            led_for(false, 2); // inter-letter gap (total 3 with prior 1)
        }
        first = false;
        // Check for a status change between letters; abort if it changed.
        if let Some(s) = poll_status() {
            if s != current {
                return Some(s);
            }
        }
        blink_letter(letter);
    }
    let _ = CMD.try_send(Cmd::SetLed(false));
    None
}

/// Blink an arbitrary user message once. Characters with no Morse mapping are
/// skipped; spaces become a word gap (7 units). Not abortable — messages are
/// short and the user asked to see the whole thing.
fn blink_message(msg: &str) {
    let _ = CMD.try_send(Cmd::SetLed(false));
    block_for(UNIT * 3); // small lead-in gap so the start is distinct

    let mut prev_was_char = false;
    for c in msg.chars() {
        if c == ' ' {
            led_for(false, 7); // inter-word gap
            prev_was_char = false;
            continue;
        }
        if let Some(code) = morse_for(c) {
            if prev_was_char {
                led_for(false, 3); // inter-letter gap
            }
            blink_letter(code);
            prev_was_char = true;
        }
    }
    let _ = CMD.try_send(Cmd::SetLed(false));
    block_for(UNIT * 3); // trailing gap before status resumes
}

/// Non-blocking poll for a status update from core0.
fn poll_status() -> Option<Status> {
    match EVT.try_receive() {
        Ok(Evt::Status(s)) => Some(s),
        Err(_) => None,
    }
}

/// The Morse task (core1). Loops the current status word; when a portal message
/// arrives, blinks it once then resumes the status word.
#[embassy_executor::task]
pub async fn morse_task() -> ! {
    let mut status = Status::Portal;

    loop {
        // 1) A user message takes priority: blink it once, then continue.
        if let Ok(msg) = MSG.try_receive() {
            blink_message(msg.as_str());
            // Drain any status change that arrived meanwhile.
            if let Some(s) = poll_status() {
                status = s;
            }
            continue;
        }

        // 2) Otherwise blink the current status word once.
        if let Some(s) = poll_status() {
            status = s;
        }
        if let Some(s) = blink_status_word(word_for(status), status) {
            // Status changed mid-word: adopt it and restart immediately.
            status = s;
            continue;
        }

        // 3) Inter-word gap before repeating (still responsive to messages,
        //    which we re-check at the top of the loop).
        block_for(UNIT * 7);
    }
}
