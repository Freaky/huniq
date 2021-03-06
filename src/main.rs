use ahash::RandomState as ARandomState;
use anyhow::{anyhow, Result};
use clap::{App, Arg};
use memchr::memchr_iter;
use std::cmp::Ordering;
use std::collections::{hash_map, HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};
use std::io::{stdin, stdout, BufRead, ErrorKind, Read, Write};
use std::{default::Default, marker::PhantomData, slice};
use sysconf::page::pagesize;

/// A no-operation hasher. Used as part of the uniq implementation,
/// because in there we manually hash the data and just store the
/// hashes of the data in the hash set. No need to hash twice
#[derive(Default)]
struct IdentityHasher {
    off: u8,
    buf: [u8; 8],
}

impl Hasher for IdentityHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.off += (&mut self.buf[self.off as usize..])
            .write(bytes)
            .unwrap_or(0) as u8;
    }

    fn finish(&self) -> u64 {
        u64::from_ne_bytes(self.buf)
    }
}

/// BuildHasher for any Hasher that implements Default
#[derive(Default)]
struct BuildDefaultHasher<H: Hasher + Default>(PhantomData<H>);

impl<H: Hasher + Default> BuildHasher for BuildDefaultHasher<H> {
    type Hasher = H;

    fn build_hasher(&self) -> Self::Hasher {
        H::default()
    }
}

/// Hash the given value with the given BuildHasher. Now.
fn hash<T: BuildHasher, U: std::hash::Hash + ?Sized>(build: &T, v: &U) -> u64 {
    let mut s = build.build_hasher();
    v.hash(&mut s);
    s.finish()
}

enum Sort {
    Ascending,
    Descending,
}

/// Split the input stream into tokens at the given delimiter.
///
/// This is an alternative to BufRead::split or BufRead::read_until
/// that does not allocate or copy the tokens around much in memory
/// usually.
///
/// (Maintains an internal buffer that will be reallocated if a token larger
/// than the buffer is encountered and when there is a token at the end of
/// the buffer, it will be moved to the start before refilling the buffer,
/// but in the normal case, no allocation or copy will occur to read a token).
///
/// The tokens found will be passed to the given callback. The tokens will
/// always be terminated by the delimiter at the end. Even if the last token
/// is delimited by eof and not the delimiter, a delimiter will be supplied.
fn split_read_zerocopy<R, F>(delim: u8, inp: &mut R, mut handle_line: F) -> Result<()>
where
    R: Read,
    F: FnMut(&[u8]) -> Result<()>,
{
    // Initialize the buffer
    let mut buf = vec![0u8; pagesize() * 2]; // buf.capacity() == buf.len() == pagesize() * 2

    // Amount of data actually in the buffer (manually managed so we
    // can avoid reinitializing all the data when we resize)
    let mut used: usize = 0;

    loop {
        match inp.read(&mut buf[used..] /* unused space */) {
            Err(ref e) if e.kind() == ErrorKind::Interrupted =>
            // EINTR – thrown when a process receives a signal…
            {
                continue
            }
            Err(e) => return Err(anyhow::Error::from(e)),
            Ok(0) => {
                // EOF; we potentially need to process the last word here
                // if the input is missing a newline (or rather delim) at
                // the end of it's input
                if used != 0 {
                    // Grow the buffer if this is necessary to insert the delimiter
                    if used == buf.len() {
                        buf.push(delim);
                    } else {
                        buf[used] = delim;
                    }
                    used += 1;

                    handle_line(&buf[..used])?;
                }

                break;
            }
            Ok(len) =>
            // Register the data that became available
            {
                used += len
            }
        };

        // Scan the buffer for lines
        let mut line_start: usize = 0;
        for off in memchr_iter(delim, &buf[..used]) {
            handle_line(&buf[line_start..off + 1])?;
            line_start = off + 1;
        }

        // Move the current line fragment to the start of the buffer
        // so we can fill the rest
        buf.copy_within(line_start.., 0);
        used = used - line_start; // Length of the rest

        // Grow the buffer if necessary, letting Vec decide what growth
        // factor to use
        if used == buf.len() {
            buf.resize(buf.len() + 1, 0);
            buf.resize(buf.capacity(), 0);
        }
    }

    Ok(())
}

/// Remove duplicates from stdin and print to stdout, counting
/// the number of occurrences.
fn count_cmd(delim: u8, sort: Option<Sort>) -> Result<()> {
    let mut set = HashMap::<Vec<u8>, u64, ARandomState>::default();
    for line in stdin().lock().split(delim) {
        match set.entry(line?) {
            hash_map::Entry::Occupied(mut e) => {
                *e.get_mut() += 1;
            }
            hash_map::Entry::Vacant(e) => {
                e.insert(1);
            }
        }
    }

    if let Some(sort) = sort {
        sort_and_print(delim, sort, &set)
    } else {
        print_out(delim, set.iter().map(|(k, v)| (k.as_slice(), *v)))
    }
}

type DataAndCount<'a> = (&'a [u8], u64);

/// Sorts the lines by occurence, then prints them
// TODO: this could be done more efficiently by reusing the memory of the HashMap
fn sort_and_print(delim: u8, sort: Sort, set: &HashMap<Vec<u8>, u64, ARandomState>) -> Result<()> {
    let mut seq: Vec<DataAndCount> = set.iter().map(|(k, v)| (k.as_slice(), *v)).collect();

    let comparator: fn(&DataAndCount, &DataAndCount) -> Ordering = match sort {
        Sort::Ascending => |a, b| a.1.cmp(&b.1),
        Sort::Descending => |a, b| b.1.cmp(&a.1),
    };
    seq.as_mut_slice().sort_by(comparator);
    print_out(delim, seq)
}

/// Prints the sequence of counts and data items, separated by delim
fn print_out<'a, I>(delim: u8, data: I) -> Result<()>
where
    I: IntoIterator<Item = DataAndCount<'a>>,
{
    let out = stdout();
    let mut out = out.lock();
    for (line, count) in data {
        write!(out, "{} ", count)?;
        out.write(&line)?;
        out.write(slice::from_ref(&delim))?;
    }

    Ok(())
}

/// Remove duplicates from stdin and print to stdout.
fn uniq_cmd(delim: u8) -> Result<()> {
    // Line processing/output ///////////////////////
    let out = stdout();
    let inp = stdin();
    let hasher = ARandomState::new();
    let mut out = out.lock();
    let mut set = HashSet::<u64, BuildDefaultHasher<IdentityHasher>>::default();

    split_read_zerocopy(delim, &mut inp.lock(), |line| {
        let tok: &[u8] = &line[..line.len() - 1];
        if set.insert(hash(&hasher, &tok)) {
            out.write(&line)?;
        }
        Ok(())
    })?;

    Ok(())
}

fn try_main() -> Result<()> {
    let mut argspec = App::new("huniq")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Remove duplicates from stdin, using a hash table")
        .author("Karolin Varner <karo@cupdev.net)")
        .arg(
            Arg::with_name("count")
                .help("Output the amount of times a line was encountered")
                .long("count")
                .short("c"),
        )
        .arg(
            Arg::with_name("sort")
                .help("Sort output by the number of occurences, in ascending order")
                .long("sort")
                .short("s"),
        )
        .arg(
            Arg::with_name("sort-descending")
                .help("Order output by the number of occurences, in descending order")
                .long("sort-descending")
                .short("S"),
        )
        .arg(
            Arg::with_name("delimiter")
                .help("Which delimiter between elements to use. By default `\n` is used")
                .long("delimiter")
                .long("delim")
                .short("d")
                .takes_value(true)
                .default_value("\n")
                .validator(|v| match v.len() {
                    1 => Ok(()),
                    _ => Err(String::from(
                        "\
Only ascii characters are supported as delimiters. \
Use sed to turn your delimiter into zero bytes?

    $ echo -n \"1λ1λ2λ3\" | sed 's@λ@\x00@g' | huniq -0 | sed 's@\x00@λ@g'
    1λ2λ3λ",
                    )),
                }),
        )
        .arg(
            Arg::with_name("null")
                .help("Use the \\0 character as the record delimiter.")
                .long("null")
                .short("0")
                .conflicts_with("delimiter"),
        );

    let args = argspec.get_matches_from_safe_borrow(&mut std::env::args_os())?;

    let delim = match args.is_present("null") {
        true => b'\0',
        false => args.value_of("delimiter").unwrap().as_bytes()[0],
    };

    let sort = match (args.is_present("sort"), args.is_present("sort-descending")) {
        (true, true) => return Err(anyhow!("cannot specify both --sort and --sort-descending")),
        (true, false) => Some(Sort::Ascending),
        (false, true) => Some(Sort::Descending),
        (false, false) => None,
    };

    match args.is_present("count") || sort.is_some() {
        true => count_cmd(delim, sort),
        false => uniq_cmd(delim),
    }
}

fn main() {
    if let Err(er) = try_main() {
        println!("{}", er);
    }
}
