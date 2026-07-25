use std::sync::LazyLock;

use rand::RngExt;

static WORDS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    let words = include_str!("words.txt").lines().collect::<Vec<_>>();
    assert_eq!(words.len(), 330);
    words
});

pub fn generate() -> String {
    let mut rng = rand::rng();
    loop {
        let words: [&str; 3] = std::array::from_fn(|_| WORDS[rng.random_range(..WORDS.len())]);
        let initials = words.map(|word| word.as_bytes()[0]);
        if initials[0] != initials[1] && initials[0] != initials[2] && initials[1] != initials[2] {
            return words.join(" ");
        }
    }
}
