// Prints from Greeter::greet a few calls deep, with a Vec and a String for
// Rust's pretty printers.
struct Greeter {
    name: String,
    counts: Vec<usize>,
}

impl Greeter {
    #[inline(never)]
    fn greet(&mut self, who: &str) {
        self.counts.push(who.len());
        println!("hello from rust, {} {} {:?}", who, self.name, self.counts);
    }
}

fn main() {
    let mut g = Greeter {
        name: "rust".to_string(),
        counts: vec![1, 2, 3],
    };
    for who in ["alice", "bob"] {
        g.greet(who);
    }
}
