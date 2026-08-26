use aiu::cli::{self, Command};
use aiu::paths;
use aiu::report::{self, detail};
use aiu::store::Store;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(args) {
        Ok(Command::Help) => print!("{}", cli::usage()),
        Ok(Command::Version) => println!("aiu {}", env!("CARGO_PKG_VERSION")),
        Ok(Command::Report { json }) => {
            if let Err(e) = run_report(json) {
                die(1, e);
            }
        }
        Ok(Command::Detail { source, json }) => {
            if let Err(e) = run_detail(&source, json) {
                die(1, e);
            }
        }
        Err(e) => die(2, e),
    }
}

fn die(code: i32, err: impl std::fmt::Display) -> ! {
    eprintln!("aiu: {err}");
    std::process::exit(code)
}

fn open_store() -> Result<Store, Box<dyn std::error::Error>> {
    let db_path = paths::db_path().ok_or(aiu::error::AiuError::NoDataDir)?;
    Ok(Store::open(&db_path)?)
}

fn run_report(json_format: bool) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_store()?;
    let report = report::build(&store, aiu::utc::now_epoch())?;
    if json_format {
        print!("{}", report::json::render(&report));
    } else {
        print!("{}", report::text::render(&report));
    }
    Ok(())
}

fn run_detail(source: &str, json_format: bool) -> Result<(), Box<dyn std::error::Error>> {
    let store = open_store()?;
    let detail = detail::build(&store, source, aiu::utc::now_epoch())?;
    if json_format {
        print!("{}", detail::json::render(&detail));
    } else {
        print!("{}", detail::text::render(&detail));
    }
    Ok(())
}
