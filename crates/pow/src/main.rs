use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage:\n  pow-cli hash <base> <nonce>\n  pow-cli solve <base> <challenge_hex> <difficulty>";

    match (args.get(1).map(String::as_str), args.len()) {
        (Some("hash"), 4) => {
            let nonce: u64 = args[3].parse().expect("nonce must be u64");
            let d = pow::deepseek_hash(&args[2], nonce);
            println!(
                "{}",
                d.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
        }
        (Some("solve"), 5) => {
            let difficulty: u64 = args[4].parse().expect("difficulty must be u64");
            match pow::solve_hex(&args[2], &args[3], difficulty) {
                Some(n) => println!("{n}"),
                None => println!("-1"),
            }
        }
        _ => {
            eprintln!("{usage}");
            exit(2);
        }
    }
}
