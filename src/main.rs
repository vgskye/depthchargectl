use std::{
    io::{Seek, SeekFrom, Write},
    path::PathBuf,
};

use block_devs::BlckExt;
use clap::{Parser, Subcommand};
use color_eyre::eyre::eyre;
use gpt::{disk::LogicalBlockSize, partition_types::CHROME_KERNEL};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    disk: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Show,
    Install {
        img: PathBuf,
        #[arg(short, long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..15))]
        tries: u8,
    },
    Confirm,
}

struct Part {
    id: u32,
    successful_boot: bool,
    tries_remaining: u64,
    priority: u64,
    start: u64,
    length: u64,
}

fn main() -> color_eyre::Result<()> {
    let args = Args::parse();

    match args.command {
        Command::Show => {
            let file = std::fs::File::open(args.disk)?;
            let disk = gpt::GptConfig::new()
                .writable(false)
                .logical_block_size(LogicalBlockSize::try_from(file.get_size_of_block()?)?)
                .open_from_device(file)?;
            for (&id, part) in disk.partitions() {
                if part.part_type_guid == CHROME_KERNEL {
                    let successful_boot = part.flags & (1 << 56) != 0;
                    let tries_remaining = (part.flags >> 52) & 0b1111;
                    let priority = (part.flags >> 48) & 0b1111;
                    println!(
                        "Partition {id}: successful_boot={successful_boot}, tries_remaining={tries_remaining}, priority={priority}, name={}",
                        part.name
                    );
                }
            }
        }
        Command::Install { img, tries } => {
            let file = std::fs::File::options()
                .read(true)
                .write(true)
                .create(false)
                .open(&args.disk)?;
            let block_size = LogicalBlockSize::try_from(file.get_size_of_block()?)?;
            let mut disk = gpt::GptConfig::new()
                .writable(true)
                .logical_block_size(block_size)
                .open_from_device(file)?;
            let mut parts = disk.take_partitions();
            let mut best_part: Option<Part> = None;
            for (&id, part) in &parts {
                if part.part_type_guid == CHROME_KERNEL {
                    let successful_boot = part.flags & (1 << 56) != 0;
                    let tries_remaining = (part.flags >> 52) & 0b1111;
                    let priority = (part.flags >> 48) & 0b1111;
                    let cur_part = Part {
                        id,
                        successful_boot,
                        tries_remaining,
                        priority,
                        start: part.bytes_start(block_size)?,
                        length: part.bytes_len(block_size)?,
                    };
                    best_part = Some(match best_part {
                        Some(best_part) => match (best_part.successful_boot, successful_boot) {
                            (true, true) | (false, false) => {
                                if best_part.priority > priority {
                                    cur_part
                                } else {
                                    best_part
                                }
                            }
                            (false, true) => best_part,
                            (true, false) => cur_part,
                        },
                        None => cur_part,
                    });
                }
            }
            let best_part = best_part.ok_or(eyre!(
                "Could not find any ChromeOS Kernel partitions in destination device!"
            ))?;
            let mut prio_set = 1u16;
            for (&id, part) in &mut parts {
                if part.part_type_guid == CHROME_KERNEL {
                    let priority = (part.flags >> 48) & 0b1111;
                    if id == best_part.id {
                        let flagmask = !((1u64 << 56) | (0b1111u64 << 52) | (0b1111u64 << 48));
                        part.flags = (part.flags & flagmask) | ((tries as u64) << 52);
                    } else {
                        prio_set |= 1 << priority;
                    }
                }
            }
            for (&id, part) in &mut parts {
                if part.part_type_guid == CHROME_KERNEL {
                    let priority = (part.flags >> 48) & 0b1111;
                    if id == best_part.id {
                        part.flags |= (prio_set.count_ones() as u64).min(15) << 48;
                    } else {
                        let new_priority = (prio_set & ((1 << priority) - 1)).count_ones() as u64;
                        let flagmask = !(0b1111u64 << 48);
                        part.flags = (part.flags & flagmask) | (new_priority << 48);
                    }
                }
            }
            disk.update_partitions(parts)?;
            let mut file = disk.write()?;
            file.seek(SeekFrom::Start(best_part.length))?;
            let img = std::fs::read(img)?;
            if img.len() as u64 > best_part.length {
                return Err(eyre!(
                    "Partition is too small (is {} bytes, needs {} bytes)",
                    best_part.length,
                    img.len()
                ));
            }
            file.write_all(&img)?;
            file.sync_all()?;
        }
        Command::Confirm => {
            let cmdline = std::fs::read_to_string("/proc/cmdline")?;
            let kernguid_start = cmdline
                .find("kern_guid=")
                .ok_or(eyre!("Could not determine current booted partition"))?;
            let kernguid = cmdline
                .get(kernguid_start + 10..kernguid_start + 46)
                .ok_or(eyre!("Could not determine current booted partition"))?;
            let kernguid: Uuid = kernguid.parse()?;
            let file = std::fs::File::options()
                .read(true)
                .write(true)
                .create(false)
                .open(&args.disk)?;
            let block_size = LogicalBlockSize::try_from(file.get_size_of_block()?)?;
            let mut disk = gpt::GptConfig::new()
                .writable(true)
                .logical_block_size(block_size)
                .open_from_device(file)?;
            let mut parts = disk.take_partitions();
            let part = parts
                .values_mut()
                .find(|e| e.part_guid == kernguid)
                .ok_or(eyre!("Current booted partition is not in this disk?"))?;
            let successful_boot = part.flags & (1 << 56) != 0;
            let tries_remaining = (part.flags >> 52) & 0b1111;
            let priority = (part.flags >> 48) & 0b1111;
            if tries_remaining == 0 && priority == 0 && !successful_boot {
                // hacky, but we don't know what the original priority was
                // next Install invocation will lower this down anyways
                part.flags |= 0b1111u64 << 48;
            }
            part.flags |= 1 << 56;
            disk.update_partitions(parts)?;
            disk.write()?;
        }
    }
    Ok(())
}
