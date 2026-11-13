//! The part of the shim that rewrites what a multiplexer would drop.
//!
//! The filter reads the output of the program, in chunks of any size, and
//! writes the same bytes, with three exceptions (all OSC sequences):
//!
//! * `OSC 7501 ; ?` (the question "does this terminal understand the program
//!   status protocol?") is removed, and the answer is put in `replies`: the
//!   program must get it from the shim, because tmux has no way to carry the
//!   answer of the outer terminal back to it.
//! * `OSC 7501 ; ...` (the status report), `OSC 9 ; 4 ; ...` (progress),
//!   `OSC 9 ; 9 ; ...` and `OSC 7 ; ...` (the working folder) are wrapped for
//!   the multiplexer, which then forwards them to the outer terminal:
//!   tmux `ESC P tmux ; <the sequence, every ESC doubled> ESC \`,
//!   screen `ESC P <the sequence, every ESC doubled> ESC \`.
//! * Nothing else is touched, not even another OSC. A sequence the program
//!   wrapped itself is a DCS, so it is never wrapped twice.
//!
//! Speed: the bytes that are not part of an OSC are copied in one piece (a
//! `memchr` for ESC), and only the first bytes of an OSC are held back, until
//! it is clear whether it is one of the above. The filter holds at most
//! `MAX_HELD` bytes, and never needs to know the size of a chunk.

use memchr::memchr;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// The longest sequence that is held back to be rewritten. A longer one is
/// passed on unchanged.
pub const MAX_HELD: usize = 4096;

/// What the program gets as the answer to `OSC 7501 ; ?` (the answer that the
/// terminal of Kennel gives).
pub const QUERY_REPLY: &[u8] =
    b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\";

/// How the sequences are wrapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrap {
    /// Not at all (not inside a multiplexer).
    None,
    Tmux,
    Screen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Between sequences.
    Normal,
    /// After an ESC (held back).
    Esc,
    /// In an OSC: after `ESC ]`.
    Osc,
    /// In an OSC, after an ESC (the start of the terminator `ESC \`).
    OscEsc,
    /// In a DCS, APC, PM or SOS string (`ESC P`, `ESC _`, `ESC ^`, `ESC X`):
    /// copied as it is until its terminator. A program that wrapped a sequence
    /// for tmux itself (a DCS with every ESC doubled) is in here, so it is not
    /// wrapped twice; so are the graphics of kitty (APC).
    Dcs,
    /// In such a string, after an ESC.
    DcsEsc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// Not enough of the sequence yet.
    Undecided,
    /// Not one of ours: send it on as it is.
    Pass,
    /// A report: wrap it.
    Wrap,
    /// The support question: answer it.
    Query,
}

/// Decides what to do with an OSC from its first bytes (what follows `ESC ]`).
fn classify(body: &[u8]) -> Verdict {
    // The sequences that are wrapped, apart from 7501 (see below).
    const WRAPPED: [&[u8]; 3] = [b"9;4;", b"9;9;", b"7;"];

    if body.starts_with(b"7501;") {
        return match body.get(5) {
            None => Verdict::Undecided,
            Some(b'?') => Verdict::Query,
            Some(_) => Verdict::Wrap,
        };
    }
    if WRAPPED.iter().any(|p| body.starts_with(p)) {
        return Verdict::Wrap;
    }
    // Still the start of one of them?
    if b"7501;".starts_with(body) || WRAPPED.iter().any(|p| p.starts_with(body)) {
        Verdict::Undecided
    } else {
        Verdict::Pass
    }
}

pub struct Filter {
    wrap: Wrap,
    answer_query: bool,
    state: State,
    /// The sequence that is being read, from its ESC, held back.
    held: Vec<u8>,
    verdict: Verdict,
}

impl Filter {
    /// `answer_query`: answer `OSC 7501 ; ?` (and do not pass it on).
    pub fn new(wrap: Wrap, answer_query: bool) -> Self {
        Filter {
            wrap,
            answer_query,
            state: State::Normal,
            held: Vec::with_capacity(256),
            verdict: Verdict::Undecided,
        }
    }

    /// Filters `input`: the bytes for the terminal are appended to `out`, the
    /// bytes for the program (answers) to `replies`.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>, replies: &mut Vec<u8>) {
        let mut i = 0;
        while i < input.len() {
            match self.state {
                State::Normal => match memchr(ESC, &input[i..]) {
                    None => {
                        out.extend_from_slice(&input[i..]);
                        return;
                    }
                    Some(p) => {
                        out.extend_from_slice(&input[i..i + p]);
                        i += p + 1;
                        self.held.clear();
                        self.held.push(ESC);
                        self.state = State::Esc;
                    }
                },
                State::Dcs => match memchr(ESC, &input[i..]) {
                    None => {
                        out.extend_from_slice(&input[i..]);
                        return;
                    }
                    Some(p) => {
                        out.extend_from_slice(&input[i..=i + p]);
                        i += p + 1;
                        self.state = State::DcsEsc;
                    }
                },
                State::DcsEsc => {
                    let b = input[i];
                    i += 1;
                    out.push(b);
                    // `ESC \` ends the string. `ESC ESC` is an ESC inside it (how
                    // tmux wants it). Another byte after the ESC should end the
                    // string too, but a string that is not ended only costs the
                    // rewrite, never a byte, so it stays lenient.
                    self.state = if b == b'\\' {
                        State::Normal
                    } else {
                        State::Dcs
                    };
                }
                State::Esc => {
                    let b = input[i];
                    if b == b']' {
                        self.held.push(b']');
                        self.state = State::Osc;
                        self.verdict = Verdict::Undecided;
                        i += 1;
                    } else if matches!(b, b'P' | b'_' | b'^' | b'X') {
                        out.push(ESC);
                        out.push(b);
                        self.held.clear();
                        self.state = State::Dcs;
                        i += 1;
                    } else {
                        // Not an OSC. The byte is read again, it may be an ESC.
                        self.release(out);
                    }
                }
                State::Osc => {
                    let b = input[i];
                    i += 1;
                    self.held.push(b);
                    if b == BEL {
                        self.complete(out, replies);
                    } else if b == ESC {
                        self.state = State::OscEsc;
                    } else {
                        self.update_verdict(out);
                    }
                }
                State::OscEsc => {
                    if input[i] == b'\\' {
                        i += 1;
                        self.held.push(b'\\');
                        self.complete(out, replies);
                    } else {
                        // An ESC that is not followed by a backslash ends the
                        // OSC and starts another sequence: send what came
                        // before it, and read the ESC again.
                        self.held.pop();
                        out.extend_from_slice(&self.held);
                        self.held.clear();
                        self.held.push(ESC);
                        self.state = State::Esc;
                    }
                }
            }
        }
    }

    /// Something is held back that may turn out to be nothing (an ESC alone, or
    /// the start of an OSC that is not known yet).
    pub fn has_undecided(&self) -> bool {
        match self.state {
            State::Normal | State::Dcs | State::DcsEsc => false,
            State::Esc => true,
            State::Osc | State::OscEsc => self.verdict == Verdict::Undecided,
        }
    }

    /// Sends on what is undecided, for when the program has been quiet for a
    /// moment. The bytes are the same as the input, so nothing is lost, only
    /// the rewrite of a sequence that comes in pieces a long time apart.
    pub fn flush_undecided(&mut self, out: &mut Vec<u8>) {
        if self.has_undecided() {
            self.release(out);
        }
    }

    /// The program ended: sends on everything that is held back.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        self.release(out);
    }

    /// Sends the held bytes on as they are, and goes back to the start.
    fn release(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.held);
        self.held.clear();
        self.state = State::Normal;
        self.verdict = Verdict::Undecided;
    }

    /// After a byte of the OSC: decides, or gives up on a sequence that is too
    /// long.
    fn update_verdict(&mut self, out: &mut Vec<u8>) {
        if self.held.len() > MAX_HELD {
            self.release(out);
            return;
        }
        if self.verdict != Verdict::Undecided {
            return;
        }
        let verdict = match classify(&self.held[2..]) {
            Verdict::Wrap if self.wrap == Wrap::None => Verdict::Pass,
            Verdict::Query if !self.answer_query => Verdict::Pass,
            v => v,
        };
        match verdict {
            Verdict::Pass => {
                // The rest of the sequence goes through the fast path.
                self.release(out);
            }
            v => self.verdict = v,
        }
    }

    /// The terminator of the OSC was read.
    fn complete(&mut self, out: &mut Vec<u8>, replies: &mut Vec<u8>) {
        match self.verdict {
            Verdict::Wrap => {
                match self.wrap {
                    Wrap::Tmux => out.extend_from_slice(b"\x1bPtmux;"),
                    Wrap::Screen => out.extend_from_slice(b"\x1bP"),
                    Wrap::None => {}
                }
                for &b in &self.held {
                    out.push(b);
                    if b == ESC {
                        out.push(ESC);
                    }
                }
                if self.wrap != Wrap::None {
                    out.extend_from_slice(b"\x1b\\");
                }
            }
            Verdict::Query => replies.extend_from_slice(QUERY_REPLY),
            Verdict::Pass | Verdict::Undecided => out.extend_from_slice(&self.held),
        }
        self.held.clear();
        self.state = State::Normal;
        self.verdict = Verdict::Undecided;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Filters `chunks` one after the other, and finishes.
    fn run(wrap: Wrap, answer: bool, chunks: &[&[u8]]) -> (Vec<u8>, Vec<u8>) {
        let mut filter = Filter::new(wrap, answer);
        let (mut out, mut replies) = (Vec::new(), Vec::new());
        for chunk in chunks {
            filter.feed(chunk, &mut out, &mut replies);
        }
        filter.finish(&mut out);
        (out, replies)
    }

    fn tmux(input: &[u8]) -> Vec<u8> {
        run(Wrap::Tmux, true, &[input]).0
    }

    #[test]
    fn plain_text_is_not_touched() {
        let input = "hello \u{1F600} world\r\n".as_bytes();
        assert_eq!(tmux(input), input);
    }

    #[test]
    fn report_with_bel_is_wrapped_for_tmux() {
        assert_eq!(
            tmux(b"\x1b]7501;state=working\x07"),
            b"\x1bPtmux;\x1b\x1b]7501;state=working\x07\x1b\\"
        );
    }

    #[test]
    fn report_with_st_has_both_escapes_doubled() {
        assert_eq!(
            tmux(b"\x1b]7501;state=done\x1b\\"),
            b"\x1bPtmux;\x1b\x1b]7501;state=done\x1b\x1b\\\x1b\\"
        );
    }

    #[test]
    fn report_is_wrapped_for_screen() {
        let (out, _) = run(Wrap::Screen, true, &[b"\x1b]7501;state=idle\x07"]);
        assert_eq!(out, b"\x1bP\x1b\x1b]7501;state=idle\x07\x1b\\");
    }

    #[test]
    fn progress_and_folder_are_wrapped() {
        for seq in [
            &b"\x1b]9;4;1;40\x07"[..],
            b"\x1b]9;9;C:\\x\x07",
            b"\x1b]7;file://host/tmp\x07",
        ] {
            let mut want = b"\x1bPtmux;".to_vec();
            for &b in seq {
                want.push(b);
                if b == ESC {
                    want.push(ESC);
                }
            }
            want.extend_from_slice(b"\x1b\\");
            assert_eq!(tmux(seq), want, "{:?}", String::from_utf8_lossy(seq));
        }
    }

    #[test]
    fn query_is_answered_and_removed() {
        for q in [
            &b"\x1b]7501;?\x1b\\"[..],
            b"\x1b]7501;?\x07",
            b"\x1b]7501;?:x\x07",
        ] {
            let (out, replies) = run(Wrap::Tmux, true, &[q]);
            assert!(out.is_empty());
            assert_eq!(replies, QUERY_REPLY);
        }
    }

    #[test]
    fn query_is_passed_on_when_not_answered() {
        let (out, replies) = run(Wrap::Tmux, false, &[b"\x1b]7501;?\x07"]);
        assert_eq!(out, b"\x1b]7501;?\x07");
        assert!(replies.is_empty());
    }

    #[test]
    fn other_sequences_are_not_touched() {
        for seq in [
            &b"\x1b]0;a title\x07"[..],
            b"\x1b]2;a title\x1b\\",
            b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07",
            b"\x1b]52;c;aGVsbG8=\x07",
            b"\x1b]11;?\x07",
            b"\x1b]7\x07",
            b"\x1b]7501\x07",
            b"\x1b]9;4\x07",
            b"\x1b[31mred\x1b[0m",
            b"\x1b7\x1b8",
            b"\x1b",
            b"\x1b\x1b",
        ] {
            assert_eq!(tmux(seq), seq, "{:?}", String::from_utf8_lossy(seq));
        }
    }

    #[test]
    fn a_wrapped_sequence_is_not_wrapped_again() {
        let seq = b"\x1bPtmux;\x1b\x1b]7501;state=working\x07\x1b\\";
        assert_eq!(tmux(seq), seq);
    }

    #[test]
    fn other_strings_are_passed_on_whole() {
        // A graphics command of kitty (APC) and a DCS, with a report inside: it is
        // data of the string, not a sequence.
        for seq in [
            &b"\x1b_Gf=100;data\x1b]7501;state=working\x07\x1b\\after"[..],
            b"\x1bP1$r0m\x1b\\\x1b]7501;state=done\x07",
        ] {
            let out = tmux(seq);
            if seq.ends_with(b"after") {
                assert_eq!(out, seq);
            } else {
                // The DCS is kept, the report after it is wrapped.
                assert!(out.starts_with(b"\x1bP1$r0m\x1b\\"));
                assert!(out.ends_with(b"state=done\x07\x1b\\"));
                assert!(out.windows(7).any(|w| w == b"\x1bPtmux;"));
            }
        }
    }

    #[test]
    fn text_around_a_report_is_kept() {
        assert_eq!(
            tmux(b"abc\x1b]7501;state=working\x07def"),
            b"abc\x1bPtmux;\x1b\x1b]7501;state=working\x07\x1b\\def"
        );
    }

    #[test]
    fn an_esc_before_a_report_is_a_sequence_of_its_own() {
        assert_eq!(
            tmux(b"\x1b\x1b]7501;state=idle\x07"),
            b"\x1b\x1bPtmux;\x1b\x1b]7501;state=idle\x07\x1b\\"
        );
    }

    #[test]
    fn an_osc_that_is_cut_by_an_esc_is_sent_as_it_is() {
        let input = b"\x1b]7501;state=wor\x1b[0m";
        assert_eq!(tmux(input), input);
    }

    #[test]
    fn a_sequence_that_is_too_long_is_sent_as_it_is() {
        let mut input = b"\x1b]7501;title=".to_vec();
        input.extend(std::iter::repeat(b'a').take(MAX_HELD * 2));
        input.push(BEL);
        assert_eq!(tmux(&input), input);
    }

    #[test]
    fn without_a_multiplexer_only_the_query_is_handled() {
        let (out, replies) = run(
            Wrap::None,
            true,
            &[b"\x1b]7501;state=working\x07\x1b]7501;?\x07"],
        );
        assert_eq!(out, b"\x1b]7501;state=working\x07");
        assert_eq!(replies, QUERY_REPLY);
    }

    #[test]
    fn the_result_does_not_depend_on_how_the_input_is_cut() {
        let inputs: [&[u8]; 6] = [
            b"abc\x1b]7501;state=working:progress=40\x07def\x1b[1mbold\x1b[0m",
            b"\x1b]7501;state=done\x1b\\x\x1b]9;4;1;40\x07y\x1b]0;t\x07",
            b"\x1b]7501;?\x1b\\\x1b]7501;?:x\x07tail",
            b"\x1bPtmux;\x1b\x1b]7501;state=working\x07\x1b\\\x1b]7;file:///a\x07",
            b"\x1b]7501;state=wor\x1b[0m\x1b\x1b]7501;state=idle\x07",
            "\u{1F600}\x1b]7501;state=blocked:kind=question\x07\u{1F600}".as_bytes(),
        ];
        for input in inputs {
            let whole = run(Wrap::Tmux, true, &[input]);
            for a in 0..=input.len() {
                for b in a..=input.len() {
                    let cut = run(Wrap::Tmux, true, &[&input[..a], &input[a..b], &input[b..]]);
                    assert_eq!(cut, whole, "cut at {a} and {b} of {:?}", input);
                }
            }
            // One byte at a time.
            let bytes: Vec<&[u8]> = input.chunks(1).collect();
            assert_eq!(run(Wrap::Tmux, true, &bytes), whole);
        }
    }

    #[test]
    fn what_is_undecided_can_be_flushed() {
        let mut filter = Filter::new(Wrap::Tmux, true);
        let (mut out, mut replies) = (Vec::new(), Vec::new());
        filter.feed(b"x\x1b", &mut out, &mut replies);
        assert_eq!(out, b"x");
        assert!(filter.has_undecided());
        filter.flush_undecided(&mut out);
        assert_eq!(out, b"x\x1b");
        assert!(!filter.has_undecided());

        // A report that is decided waits for its end: it is not undecided.
        filter.feed(b"\x1b]7501;state=wor", &mut out, &mut replies);
        assert!(!filter.has_undecided());
        filter.feed(b"king\x07", &mut out, &mut replies);
        assert!(out.ends_with(b"working\x07\x1b\\"));
    }
}
