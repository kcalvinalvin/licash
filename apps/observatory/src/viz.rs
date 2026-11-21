//! The small pictures of the pages, as HTML and SVG with no script: a
//! meter (a bar cut in parts), an uptime strip and a row of status dots.
//! Each part carries a `title` and each picture its words beside it, so
//! that none is the only way to read a value.

use std::fmt::Write as _;

use crate::{format::esc, view::Bucket};

/// A part of a [`meter`]: its style class, its share of the full bar (0 to
/// 1) and what it says when hovered.
pub struct Part<'a> {
    pub class: &'a str,
    pub share: f64,
    pub title: String,
}

/// A bar cut in `parts`, laid end to end from the left. Shares beyond what
/// fits are cut, and one too small to see is still drawn as a sliver, so
/// that a value never looks like none.
pub fn meter(parts: &[Part]) -> String {
    let mut left = 1.0_f64;
    let mut out = String::from("<span class=\"meter\" aria-hidden=\"true\">");
    for part in parts {
        let share = part.share.clamp(0.0, left);
        if share <= 0.0 {
            continue;
        }
        left -= share;
        let _ = write!(
            out,
            "<i class=\"{}\" style=\"width:{:.2}%\" title=\"{}\"></i>",
            part.class,
            (share * 100.0).max(0.8),
            esc(&part.title)
        );
    }
    out.push_str("</span>");
    out
}

/// A one-part meter of `share` of the full bar.
pub fn bar(class: &str, share: f64, title: &str) -> String {
    meter(&[Part {
        class,
        share,
        title: title.to_owned(),
    }])
}

/// What a bucket of probes says, as a class and in words.
fn said(bucket: &Bucket) -> (&'static str, String) {
    match (bucket.ok, bucket.total) {
        (_, 0) => ("none", "not probed".to_owned()),
        (ok, total) if ok == total => ("ok", format!("{ok} of {total} answered")),
        (0, total) => ("bad", format!("none of {total} answered")),
        (ok, total) => ("some", format!("{ok} of {total} answered")),
    }
}

/// A service's probes over the range as a strip of cells, each green if all
/// its probes answered, amber if some, red if none, and grey if none was
/// made.
pub fn strip(chart: &[Bucket], label: &str) -> String {
    const CELL: f64 = 4.0;
    const GAP: f64 = 1.0;
    let n = chart.len().max(1) as f64;
    let width = n * (CELL + GAP) - GAP;
    let mut out = format!(
        "<svg class=\"strip\" viewBox=\"0 0 {width} 16\" preserveAspectRatio=\"none\" \
         role=\"img\" aria-label=\"{}\">",
        esc(label)
    );
    for (i, bucket) in chart.iter().enumerate() {
        let (class, said) = said(bucket);
        let _ = write!(
            out,
            "<rect class=\"{class}\" x=\"{}\" y=\"0\" width=\"{CELL}\" height=\"16\" rx=\"1.5\">\
             <title>{said}</title></rect>",
            i as f64 * (CELL + GAP)
        );
    }
    out.push_str("</svg>");
    out
}

/// One dot for each of `states`: up, down, or not probed lately.
pub fn dots(states: &[Option<bool>]) -> String {
    let mut out = String::from("<span class=\"dots\" aria-hidden=\"true\">");
    for state in states {
        let class = match state {
            Some(true) => "ok",
            Some(false) => "bad",
            None => "idle",
        };
        let _ = write!(out, "<i class=\"{class}\"></i>");
    }
    out.push_str("</span>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(ok: u32, total: u32) -> Bucket {
        Bucket {
            at: 0,
            ok,
            total,
            rtt: None,
        }
    }

    #[test]
    fn a_meter_never_overflows_and_never_hides_a_small_part() {
        let html = meter(&[
            Part {
                class: "a",
                share: 0.7,
                title: "seventy".into(),
            },
            Part {
                class: "b",
                share: 0.7,
                title: "cut".into(),
            },
            Part {
                class: "c",
                share: 0.0,
                title: "none".into(),
            },
        ]);
        assert!(html.contains("width:70.00%"), "{html}");
        assert!(
            html.contains("width:30.00%"),
            "the second is cut to what is left"
        );
        assert!(!html.contains("class=\"c\""), "an empty part is not drawn");
        let tiny = bar("a", 0.0001, "tiny");
        assert!(tiny.contains("width:0.80%"), "{tiny}");
    }

    #[test]
    fn a_strip_has_a_cell_for_each_bucket_in_words() {
        let html = strip(
            &[bucket(3, 3), bucket(1, 3), bucket(0, 3), bucket(0, 0)],
            "x",
        );
        for class in ["ok", "some", "bad", "none"] {
            assert!(html.contains(&format!("class=\"{class}\"")), "{class}");
        }
        assert!(html.contains("1 of 3 answered") && html.contains("not probed"));
        assert_eq!(html.matches("<rect").count(), 4);
    }

    #[test]
    fn dots_show_each_state() {
        let html = dots(&[Some(true), Some(false), None]);
        assert_eq!(html.matches("<i ").count(), 3);
        assert!(html.contains("class=\"bad\""));
    }
}
