use crate::types::History;

type ScoredHistory = (f64, History);

pub fn sort(query: &str, input: Vec<History>) -> Vec<History> {
    let mut scored = input
        .into_iter()
        .map(|h| {
            let score = if h.command.starts_with(query) {
                2.0
            } else if h.command.contains(query) {
                1.75
            } else {
                1.0
            };

            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let time = h.timestamp.unix_timestamp();
            let diff = std::cmp::max(1, now - time);

            #[allow(clippy::cast_precision_loss)]
            let time_score = 1.0 + (1.0 / diff as f64);
            let score = score * time_score;

            (score, h)
        })
        .collect::<Vec<ScoredHistory>>();

    scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().reverse());

    scored.into_iter().map(|(_, h)| h).collect::<Vec<History>>()
}
