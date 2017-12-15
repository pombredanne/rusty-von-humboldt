extern crate rusty_von_humboldt;

extern crate serde;
extern crate serde_json;
extern crate rayon;
extern crate stopwatch;
extern crate rusoto_core;
extern crate rusoto_s3;
extern crate flate2;
extern crate rand;
extern crate md5;
#[macro_use]
extern crate lazy_static;
extern crate chrono;

use std::io::prelude::*;
use std::env;
use std::sync::mpsc::sync_channel;
use std::{thread, time};
use std::thread::JoinHandle;
use std::str::FromStr;
use rayon::prelude::*;
use flate2::Compression;
use flate2::write::GzEncoder;
use chrono::{DateTime, Utc};

use rusty_von_humboldt::*;
use rand::{thread_rng, Rng};
use rusoto_core::{DefaultCredentialsProviderSync, Region, default_tls_client, ProvideAwsCredentials, DispatchSignedRequest};
use rusoto_s3::{S3, S3Client, PutObjectRequest};


fn pipeline_main() {
    environment_check();

    let pipes = make_channels_and_threads();
    let file_list = make_list();

    // distribute file list equally into the pipeline channels
    send_ze_files(&pipes, &file_list);

    wait_for_threads(pipes);
}

fn wait_for_threads(pipes: Vec<PipelineTracker>) {
    println!("Waiting for threads to finish by sending end of work signal.");
    for pipe in &pipes {
        let done_signal = FileWorkItem {
            file: String::new(),
            no_more_work: true,
        };
        let mut done = false;
        while !done {
            match pipe.transmit_channel.try_send(done_signal.clone()) {
                Ok(_) => done = true,
                Err(e) => {
                    println!("Couldn't send to channel: {}", e);
                    thread::sleep(time::Duration::from_millis(5000));
                },
            }
        }
    }
    println!("\nSent end of work signal to all threads, waiting.\n");
    for pipe in pipes {
        match pipe.thread.join() {
            Ok(_) => println!("Pipe thread all wrapped up."),
            Err(e) => println!("Pipe thread didn't want to quit: {:?}", e),
        }
    }
}

fn send_ze_files(pipes: &[PipelineTracker], file_list: &[String]) {
    for (i, file) in file_list.iter().enumerate() {
        let mut file_sent = false;
        let item_to_send = FileWorkItem {
            file: file.clone(),
            no_more_work: false,
        };
        // Keep trying to find the first open slot
        while !file_sent {
            for pipe in pipes {
                if file_sent {
                    break;
                }  
                match pipe.transmit_channel.try_send(item_to_send.clone()) {
                    Ok(_) => file_sent = true,
                    Err(_) => (),
                }
            }
            // Is this needed?
            thread::sleep(time::Duration::from_millis(2));
        }
        // print how many ingest files we've sent off so far
        if i % 100 == 0 {
            println!("Distributed {} files to process.", i);
        }
    }
    println!("Files all sent.");
}

fn make_channels_and_threads() -> Vec<PipelineTracker> {
    let mut pipes: Vec<PipelineTracker> = Vec::new();
    let num_threads = 3;
    for _x in 0..num_threads {
        let (send, recv) = sync_channel(2);
        let thread = thread::spawn(move|| {
            let client = S3Client::new(default_tls_client().expect("Couldn't make TLS client"),
                DefaultCredentialsProviderSync::new().expect("Couldn't get new copy of DefaultCredentialsProviderSync"),
                Region::UsEast1);
            let mut wrap_things_up = false;
            let mut work_items: Vec<String> = Vec::new();
            println!("Thread {:?}, starting.", thread::current().id());
            loop {
                if wrap_things_up {
                    println!("wrapping thread up.");
                    break;
                }
                work_items.clear();
                // this loop does the accumulation of items to download, parse, convert, compress, upload:
                loop {
                    let item: FileWorkItem = match recv.recv() {
                        Ok(i) => i,
                        Err(e) => {
                            println!("Oh noe receiving error: {:?}", e);
                            panic!("receiving error");
                        },
                    };
                    if item.no_more_work {
                        println!("No more work, hooray!");
                        wrap_things_up = true;
                        break;
                    } else {
                        work_items.push(item.file);
                    }
                    if work_items.len() >= 100 {
                        println!("Got enough items, time to work.");
                        break;
                    }
                }
                if work_items.len() == 0 {
                    println!("Nothing to work on, breaking out");
                    break;
                }
                println!("{:?} calling SFoD with {} items.", thread::current().id(), work_items.len());
                single_function_of_doom(&client, &work_items);
            }
            println!("Thread {:?}, out!", thread::current().id());
        });
        let pipe = PipelineTracker {
            thread: thread,
            transmit_channel: send,
        };
        pipes.push(pipe);
    }
    pipes
}

fn compress_and_send
    <P: ProvideAwsCredentials + Sync + Send,
    D: DispatchSignedRequest + Sync + Send>
    (work_item: WorkItem, client: &S3Client<P, D>) {

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(work_item.sql.as_bytes()).expect("encoding failed");
    let compressed_results = encoder.finish().expect("Couldn't compress file, sad.");

    let upload_request = PutObjectRequest {
        bucket: work_item.s3_bucket_name.to_owned(),
        key: work_item.s3_file_location.to_owned(),
        body: Some(compressed_results),
        ..Default::default()
    };

    if MODE.dry_run {
        println!("Not uploading to S3, it's a dry run.  Would have uploaded to bucket {} and key {}.", upload_request.bucket, upload_request.key);
        return;
    }

    match client.put_object(&upload_request) {
        Ok(_) => println!("uploaded {} to {}", work_item.s3_file_location, work_item.s3_bucket_name),
        Err(_) => {
            thread::sleep(time::Duration::from_millis(100));
            match client.put_object(&upload_request) {
                Ok(_) => println!("uploaded {} to {}", work_item.s3_file_location, work_item.s3_bucket_name),
                Err(_) => {
                    thread::sleep(time::Duration::from_millis(1000));
                    match client.put_object(&upload_request) {
                        Ok(_) => println!("uploaded {} to {}", work_item.s3_file_location, work_item.s3_bucket_name),
                        Err(_) => {
                            let client = S3Client::new(default_tls_client().expect("Couldn't make TLS client"),
                                DefaultCredentialsProviderSync::new().expect("Couldn't get new copy of DefaultCredentialsProviderSync"),
                                Region::UsEast1);
                            match client.put_object(&upload_request) {
                                Ok(_) => println!("uploaded {} to {} with new client", work_item.s3_file_location, work_item.s3_bucket_name),
                                Err(e) => println!("FOURTH ATTEMPT TO UPLOAD FAILED SO SAD. {:?}", e),
                            }
                        },
                    };
                },
            };
        }
    }
}

fn generate_mode_string() -> String {
    if MODE.committer_count {
        return "committers".to_string();
    }
    "repomapping".to_string()
}

// should be on the receiving end of a channel instead of being a function call, I think.
fn single_function_of_doom 
    <P: ProvideAwsCredentials + Sync + Send,
    D: DispatchSignedRequest + Sync + Send>
    (client: &S3Client<P, D>, chunk: &[String]) {
    let dest_bucket = env::var("DESTBUCKET").expect("Need DESTBUCKET set to bucket name");
    if MODE.committer_count {
        // TODO: extract to function
        if *YEAR < 2015 {
            let mut committer_events: Vec<CommitEvent> = get_old_event_subset_committers(chunk, &client)
                .par_iter()
                .map(|item| item.as_commit_event())
                .collect();

            let old_size = committer_events.len();
            committer_events.sort();
            committer_events.dedup();
            println!("{:?}: We shrunk the pre-2015 committer events from {} to {}", thread::current().id(), old_size, committer_events.len());

            let sql = committer_events
                .par_iter()
                .map(|item| format!("{}\n", item.as_sql()))
                .collect::<Vec<String>>()
                .join("");

            let file_name = format!("rvh/{}/{}/{:x}.txt.gz", generate_mode_string(), *YEAR, md5::compute(&sql));

            let workitem = WorkItem {
                sql: sql,
                s3_bucket_name: dest_bucket.clone(),
                s3_file_location: file_name,
                no_more_work: false,
            };

            compress_and_send(workitem, client);
        } else {
            let event_subset = get_event_subset_committers(chunk, &client);
            // println!("2015+ eventsubset is {:#?}", event_subset.first().unwrap());
            let mut committer_events: Vec<CommitEvent> = event_subset
                .par_iter()
                .map(|item| item.as_commit_event())
                .collect();
            
            let old_size = committer_events.len();
            committer_events.sort();
            committer_events.dedup();
            println!("{:?}: We shrunk the 2015+ committer events from {} to {}", thread::current().id(), old_size, committer_events.len());

            let sql = committer_events
                .par_iter()
                .map(|item| format!("{}\n", item.as_sql()))
                .collect::<Vec<String>>()
                .join("");

            let file_name = format!("rvh/{}/{}/{:x}.txt.gz", generate_mode_string(), *YEAR, md5::compute(&sql));

            let workitem = WorkItem {
                sql: sql,
                s3_bucket_name: dest_bucket.clone(),
                s3_file_location: file_name,
                no_more_work: false,
            };
            compress_and_send(workitem, client);
        }
    } else if MODE.repo_mapping {
        // TODO: extract to function
        if *YEAR < 2015 {
            // change get_old_event_subset to only fetch x number of files?
            let event_subset = get_old_event_subset(chunk, &client);
            let sql = repo_id_to_name_mappings_old(&event_subset)
                .par_iter()
                .map(|item| format!("{}\n", item.as_sql()))
                .collect::<Vec<String>>()
                .join("");
            
            let file_name = format!("rvh/{}/{:x}", generate_mode_string(), md5::compute(&sql));

            let workitem = WorkItem {
                sql: sql,
                s3_bucket_name: dest_bucket.clone(),
                s3_file_location: file_name,
                no_more_work: false,
            };
            compress_and_send(workitem, client);
        } else {
            // deduping this would be ~~~~amazing
            let event_subset = get_event_subset(chunk, &client);
            let sql = repo_id_to_name_mappings(&event_subset)
                .par_iter()
                .map(|item| format!("{}\n", item.as_sql()))
                .collect::<Vec<String>>()
                .join("");
            let file_name = format!("rvh/{}/{:x}", generate_mode_string(), md5::compute(&sql));
            let workitem = WorkItem {
                sql: sql,
                s3_bucket_name: dest_bucket.clone(),
                s3_file_location: file_name,
                no_more_work: false,
            };
            compress_and_send(workitem, client);
        }
    }
}

// check things like dryrun etc
fn environment_check() {
    let _ = env::var("DESTBUCKET").expect("Need DESTBUCKET set to bucket name");
    let _ = env::var("GHABUCKET").expect("Need GHABUCKET set to bucket name");
    let _ = env::var("GHAYEAR").expect("Need GHAYEAR set to year to process");
    let _ = env::var("GHAHOURS")
        .expect("Need GHAHOURS set to number of hours (files) to process")
        .parse::<i64>().expect("Please set GHAHOURS to an integer value");
}

fn main() {
    println!("Welcome to Rusty von Humboldt.");
    pipeline_main();
    println!("This is Rusty von Humboldt, heading home.");
}

fn make_list() -> Vec<String> {
    let mut file_list = construct_list_of_ingest_files();
    let mut rng = thread_rng();
    rng.shuffle(&mut file_list);
    println!("file list is now {:#?}", file_list);
    file_list
}

fn get_event_subset<P: ProvideAwsCredentials + Sync + Send,
    D: DispatchSignedRequest + Sync + Send>(chunk: &[String], client: &S3Client<P, D>) -> Vec<Event> {
    chunk
        .par_iter()
        // todo: don't panic here
        .flat_map(|file_name| download_and_parse_file(file_name, &client).expect("Issue with file ingest"))
        .collect()
}

fn get_event_subset_committers<P: ProvideAwsCredentials + Sync + Send,
    D: DispatchSignedRequest + Sync + Send>(chunk: &[String], client: &S3Client<P, D>) -> Vec<Event> {
    
    let commit_events: Vec<Event> = chunk
        .par_iter()
        // todo: don't panic here
        .flat_map(|file_name| download_and_parse_file(file_name, &client).expect("Issue with file ingest"))
        .filter(|ref x| x.is_commit_event())
        .collect();
    commit_events
}

fn get_old_event_subset_committers<P: ProvideAwsCredentials + Sync + Send,
    D: DispatchSignedRequest + Sync + Send>(chunk: &[String], client: &S3Client<P, D>) -> Vec<Pre2015Event> {
    
    let commit_events: Vec<Pre2015Event> = chunk
        .par_iter()
        // todo: don't panic here
        .flat_map(|file_name| download_and_parse_old_file(file_name, &client).expect("Issue with file ingest"))
        .filter(|ref x| x.is_commit_event())
        .collect();
    commit_events
}

fn get_old_event_subset<P: ProvideAwsCredentials + Sync + Send,
    D: DispatchSignedRequest + Sync + Send>(chunk: &[String], client: &S3Client<P, D>) -> Vec<Pre2015Event> {
    chunk
        .par_iter()
        // todo: don't panic here
        .flat_map(|file_name| download_and_parse_old_file(file_name, &client).expect("Issue with file ingest"))
        .collect()
}

fn repo_id_to_name_mappings_old(events: &[Pre2015Event]) -> Vec<RepoIdToName> {
    let mut repo_mappings: Vec<RepoIdToName> = events
        .par_iter()
        .map(|r| {
            // replace with r.repo_id():
            let repo_id = match r.repo {
                Some(ref repo) => repo.id,
                None => match r.repository {
                    Some(ref repository) => repository.id,
                    None => -1,
                }
            };
            let repo_name = match r.repo {
                Some(ref repo) => repo.name.clone(),
                None => match r.repository {
                    Some(ref repository) => repository.name.clone(),
                    None => "".to_string(),
                }
            };

            let timestamp = match DateTime::parse_from_rfc3339(&r.created_at) {
                Ok(time) => time,
                Err(_) => DateTime::parse_from_rfc3339("2011-01-01T21:00:09+09:00").unwrap(), // Make ourselves low priority
            };

            let utc_timestamp = DateTime::<Utc>::from_utc(timestamp.naive_utc(), Utc);

            RepoIdToName {
                    repo_id: repo_id,
                    repo_name: repo_name,
                    event_timestamp: utc_timestamp,
                }
            }
        )
        .filter(|x| x.repo_id >= 0)
        .filter(|x| x.repo_name != "")
        .collect();
    // We should try to dedupe here: convert to actual timestamps instead of doing Strings for timestamps
    // get unique list of repo ids
    repo_mappings.sort_by_key(|x| x.repo_id);
    let mut list_of_repo_ids: Vec<i64> = repo_mappings.iter().map(|x| x.repo_id).collect();
    list_of_repo_ids.sort();
    list_of_repo_ids.dedup();
    // for each repo id, find the entry with the most recent timestamp
    let a: Vec<RepoIdToName> = list_of_repo_ids
        .iter()
        .map(|repo_id| {
            // find most up to date entry for this one
            let mut all_entries_for_repo_id: Vec<RepoIdToName> = repo_mappings
                .iter()
                .filter(|x| x.repo_id == *repo_id)
                .map(|x| x.clone())
                .collect();
            all_entries_for_repo_id.sort_by_key(|x| x.event_timestamp);
            // println!("sorted: {:#?}", all_entries_for_repo_id);
            all_entries_for_repo_id.last().unwrap().clone()
        })
        .collect();

    // collect and return those most recent timestamp ones
    // println!("repo mappings after dedupin': {:#?}", a);
    println!("pre-2015 len difference: {:?} to {:?}", repo_mappings.len(), a.len());
    a
}

// We should add some testing on this
fn repo_id_to_name_mappings(events: &[Event]) -> Vec<RepoIdToName> {
    let mut repo_mappings: Vec<RepoIdToName> = events
        .par_iter()
        .map(|r| RepoIdToName {
                repo_id: r.repo.id,
                repo_name: r.repo.name.clone(),
                event_timestamp: r.created_at.clone(),
            })
        .collect();

    // println!("repo mappings at first: {:#?}", repo_mappings);

    // get unique list of repo ids
    repo_mappings.sort_by_key(|x| x.repo_id);
    let mut list_of_repo_ids: Vec<i64> = repo_mappings.par_iter().map(|x| x.repo_id).collect();
    list_of_repo_ids.sort();
    list_of_repo_ids.dedup();
    // for each repo id, find the entry with the most recent timestamp
    let a: Vec<RepoIdToName> = list_of_repo_ids
        .par_iter()
        .map(|repo_id| {
            // find most up to date entry for this one
            let mut all_entries_for_repo_id: Vec<RepoIdToName> = repo_mappings
                .iter()
                .filter(|x| x.repo_id == *repo_id)
                .map(|x| x.clone())
                .collect();
            all_entries_for_repo_id.sort_by_key(|x| x.event_timestamp);
            // println!("sorted: {:#?}", all_entries_for_repo_id);
            all_entries_for_repo_id.last().unwrap().clone()
        })
        .collect();

    // collect and return those most recent timestamp ones
    // println!("repo mappings after dedupin': {:#?}", a);
    println!("len difference: {:?} to {:?}", repo_mappings.len(), a.len());
    a
}

#[derive(Debug, Clone)]
struct WorkItem {
    sql: String,
    s3_bucket_name: String,
    s3_file_location: String,
    no_more_work: bool,
}

#[derive(Debug, Clone)]
struct Mode {
    committer_count: bool,
    repo_mapping: bool,
    dry_run: bool,
}

#[derive(Debug)]
struct PipelineTracker {
    thread: JoinHandle<()>,
    transmit_channel: std::sync::mpsc::SyncSender<FileWorkItem>,
}

#[derive(Debug, Clone)]
struct FileWorkItem {
    file: String,
    no_more_work: bool,
}

lazy_static! {
    static ref MODE: Mode = Mode { 
        committer_count: true,
        repo_mapping: false,
        dry_run: {
            match env::var("DRYRUN"){
                Ok(dryrun) => match bool::from_str(&dryrun) {
                    Ok(should_dryrun) => should_dryrun,
                    Err(_) => false,
                },
                Err(_) => false,  
            }
        },
    };
}

lazy_static! {
    static ref YEAR: i32 = {
        env::var("GHAYEAR").expect("Please set GHAYEAR env var").parse::<i32>().expect("Please set GHAYEAR env var to an integer value.")
    };
}

#[cfg(test)]
mod tests {

    #[test]
    fn reduce_works() {
        use repo_id_to_name_mappings;
        use rusty_von_humboldt::RepoIdToName;
        use rusty_von_humboldt::types::Event;
        use chrono::{TimeZone, Utc};

        let most_newest_timestamp = Utc.ymd(2014, 7, 8).and_hms(9, 10, 11);
        let an_older_timestamp = Utc.ymd(2014, 7, 8).and_hms(0, 10, 11);

        let mut expected: Vec<RepoIdToName> = Vec::new();
        expected.push(RepoIdToName {
            repo_id: 5,
            repo_name: "new".to_string(),
            event_timestamp: most_newest_timestamp,
        });

        let mut input = Vec::new();
        
        let mut foo = Event::new();
        foo.repo.id = 5;
        foo.repo.name = "old".to_string();
        foo.created_at = an_older_timestamp;
        input.push(foo);

        foo = Event::new();
        foo.repo.id = 5;
        foo.repo.name = "new".to_string();
        foo.created_at = most_newest_timestamp;
        input.push(foo);

        assert_eq!(expected, repo_id_to_name_mappings(&input));
    }

    // mostly a test for playing with the different timestamps in pre-2015 events
    #[test]
    fn timestamp_parsing() {
        use chrono::{DateTime, Utc};
        let style_one = "2013-01-01T12:00:24-08:00";
        let style_two = "2011-05-01T15:59:59Z";

        match DateTime::parse_from_rfc3339(style_one) {
            Ok(time) => println!("got {:?} from {:?}", time, style_one),
            Err(e) => println!("Failed to get anything from {:?}. Error: {:?}", style_one, e),
        }

        match DateTime::parse_from_rfc3339(style_two) {
            Ok(time) => println!("got {:?} from {:?}", time, style_two),
            Err(e) => println!("Failed to get anything from {:?}. Error: {:?}", style_two, e),
        }

        let localtime = DateTime::parse_from_rfc3339(style_two).unwrap();
        let _utc: DateTime<Utc> = DateTime::<Utc>::from_utc(localtime.naive_utc(), Utc);
    }
}