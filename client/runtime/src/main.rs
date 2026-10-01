use anyhow::Result;
use freec_runtime::{start, Command};
use std::{
    io::{self, BufRead},
    time::Duration,
};

fn main() -> Result<()> {
    let app_id = std::env::var("FREECTIER_APP_ID")
        .unwrap_or_else(|_| freec_runtime::DEFAULT_APP_ID.to_string())
        .parse()?;
    let handle = start(None, app_id)?;
    println!("FreeC Tier diagnostic client · AppID {app_id}");
    println!(
        "Commands: status | create NAME | invite UUID | join LOBBY_ID | on UUID | off UUID | quit"
    );
    for line in io::stdin().lock().lines() {
        let line = line?;
        let (command, value) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        let result = (|| -> Result<()> {
            match command {
                "status" => println!("{}", serde_json::to_string_pretty(&handle.snapshot())?),
                "create" => handle.send(Command::Create {
                    name: value.to_owned(),
                    public: false,
                    password: String::new(),
                })?,
                "invite" => handle.send(Command::Invite {
                    network: value.parse()?,
                })?,
                "join" => handle.send(Command::Join {
                    lobby: value.to_owned(),
                })?,
                "on" | "off" => handle.send(Command::SetAdapter {
                    network: value.parse()?,
                    enabled: command == "on",
                })?,
                "quit" => {
                    handle.send(Command::Shutdown)?;
                }
                _ => println!("Unknown command"),
            }
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("{error:#}");
        }
        if command == "quit" {
            std::thread::sleep(Duration::from_millis(50));
            break;
        }
    }
    Ok(())
}
