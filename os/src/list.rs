use std::fs;
use std::io;

fn main() -> io::Result<()>{
    let entries = fs::read_dir(".")?;
    for entry in entries{
        let entry = entry?;
        let file_name = entry.file_name();
        println!("{}",file_name.to_string_lossy());
    }
    Ok(())
}
