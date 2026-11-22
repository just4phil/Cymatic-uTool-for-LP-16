//! Background check of WAV endings (`wav_tail::analyze_tail`) for many
//! songs at once: the "Check song endings" button in Home and the check of
//! freshly imported songs.

use eframe::egui;
use lp16_tool::wav_tail::{self, TailReport};
use std::sync::{Arc, Mutex};

/// Why a check was started; decides the message at the end.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum TailCheckKind {
    /// the button: always report, also when everything is clean
    Library,
    /// after an import: report only problems
    Import,
    /// before an export of these songs: clean = export at once, problems =
    /// ask (the caller decides; `summary` says nothing)
    Export,
}

#[derive(Default)]
struct Job {
    total: usize,
    done: usize,
    /// (wav, finding); None = the WAV ends cleanly or can't be read
    results: Vec<(String, Option<TailReport>)>,
    finished: bool,
}

/// One running check.
pub(crate) struct TailCheck {
    pub kind: TailCheckKind,
    job: Arc<Mutex<Job>>,
}

impl TailCheck {
    /// Check `wavs` on a background thread.
    pub fn start(kind: TailCheckKind, wavs: Vec<String>, ctx: &egui::Context) -> TailCheck {
        let job = Arc::new(Mutex::new(Job { total: wavs.len(), ..Default::default() }));
        let shared = job.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for (i, wav) in wavs.into_iter().enumerate() {
                let r = wav_tail::analyze_tail(&wav).ok().flatten();
                let mut j = shared.lock().unwrap();
                j.results.push((wav, r));
                j.done += 1;
                drop(j);
                if i % 16 == 0 {
                    ctx.request_repaint();
                }
            }
            shared.lock().unwrap().finished = true;
            ctx.request_repaint();
        });
        TailCheck { kind, job }
    }

    /// (done, total) WAV files
    pub fn progress(&self) -> (usize, usize) {
        let j = self.job.lock().unwrap();
        (j.done, j.total)
    }

    /// The results once the check is finished.
    pub fn take_finished(&self) -> Option<Vec<(String, Option<TailReport>)>> {
        let mut j = self.job.lock().unwrap();
        j.finished.then(|| std::mem::take(&mut j.results))
    }
}

/// The message after a check of `songs` songs, `affected` = names of the
/// songs with at least one WAV without a clean end. None = say nothing.
pub(crate) fn summary(kind: TailCheckKind, songs: usize, wavs: usize, affected: &[String]) -> Option<String> {
    if kind == TailCheckKind::Export {
        return None;
    }
    if affected.is_empty() {
        return (kind == TailCheckKind::Library)
            .then(|| format!("{} Song(s) checked ({} WAV files): all end cleanly.", songs, wavs));
    }
    let list = name_list(affected);
    let head = match kind {
        TailCheckKind::Import => format!("{} of the imported Song(s) have WAV files without a clean fade-out:", affected.len()),
        _ => format!("{} of {} Song(s) have WAV files without a clean fade-out:", affected.len(), songs),
    };
    Some(format!(
        "{}\n\n{}\n\nThey are marked with \"!\" in Home. Select a song and click \"No clean fade-out\" to repair it.",
        head, list
    ))
}

/// The question before an export when `affected` of `songs` songs have WAV
/// files without a clean end.
pub(crate) fn export_question(songs: usize, affected: &[String]) -> String {
    format!(
        "{} of the {} Song(s) to export have WAV files without a clean fade-out \
         (the LP-16 may play a click at the end):\n\n{}\n\nExport anyway?",
        affected.len(),
        songs,
        name_list(affected)
    )
}

/// Up to 15 names, one per line, indented.
fn name_list(names: &[String]) -> String {
    let mut list = names.iter().take(15).map(|n| format!("    {}", n)).collect::<Vec<_>>().join("\n");
    if names.len() > 15 {
        list += &format!("\n    … and {} more", names.len() - 15);
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_texts() {
        assert!(summary(TailCheckKind::Library, 3, 20, &[]).unwrap().contains("all end cleanly"));
        assert_eq!(summary(TailCheckKind::Import, 3, 20, &[]), None);
        let names: Vec<String> = (0..20).map(|i| format!("S{}", i)).collect();
        let t = summary(TailCheckKind::Library, 30, 200, &names).unwrap();
        assert!(t.starts_with("20 of 30 Song(s)"));
        assert!(t.contains("S14") && !t.contains("S15\n") && t.contains("and 5 more"));
        assert!(summary(TailCheckKind::Import, 3, 20, &names[..1]).unwrap().contains("1 of the imported"));
    }

    #[test]
    fn check_runs_in_the_background() {
        let ctx = egui::Context::default();
        let c = TailCheck::start(TailCheckKind::Import, vec!["does-not-exist.wav".into()], &ctx);
        let t0 = std::time::Instant::now();
        let r = loop {
            if let Some(r) = c.take_finished() {
                break r;
            }
            assert!(t0.elapsed().as_secs() < 5);
            std::thread::yield_now();
        };
        assert_eq!(r, vec![("does-not-exist.wav".to_string(), None)]);
        assert_eq!(c.progress(), (1, 1));
    }
}
