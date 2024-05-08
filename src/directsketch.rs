use anyhow::{anyhow, bail, Context, Error, Result};
use async_zip::base::write::{self, ZipFileWriter};
use async_zip::Compression;
use async_zip::{ZipDateTime, ZipEntryBuilder};
use camino::Utf8PathBuf as PathBuf;
use chrono::Utc;
use needletail::parse_fastx_reader;
use regex::Regex;
use reqwest::Client;
use std::collections::HashMap;
use std::fs::{self, create_dir_all};
use std::io::Cursor;
use std::path::Path;
use tokio::fs::File;
use tokio::task;
use tokio_util::compat::Compat;

use pyo3::prelude::*;

use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufWriter};

use tokio::sync::Semaphore;
use tokio::time::{interval, Duration};

use sourmash::manifest::{Manifest, Record};
use sourmash::signature::Signature;

use crate::utils::{build_siginfo, load_accession_info, parse_params_str};

enum GenBankFileType {
    Genomic,
    Protein,
    AssemblyReport,
    Checksum,
}

impl GenBankFileType {
    fn suffix(&self) -> &'static str {
        match self {
            GenBankFileType::Genomic => "_genomic.fna.gz",
            GenBankFileType::Protein => "_protein.faa.gz",
            GenBankFileType::AssemblyReport => "_assembly_report.txt",
            GenBankFileType::Checksum => "md5checksums.txt",
        }
    }

    fn filename(&self, accession: &str) -> String {
        match self {
            GenBankFileType::Checksum => format!("{}_{}", accession, self.suffix()),
            _ => format!("{}{}", accession, self.suffix()),
        }
    }

    fn url(&self, base_url: &str, full_name: &str) -> String {
        format!("{}/{}{}", base_url, full_name, self.suffix())
    }

    fn moltype(&self) -> String {
        match self {
            GenBankFileType::Genomic => "DNA".to_string(),
            GenBankFileType::Protein => "protein".to_string(),
            _ => "".to_string(),
        }
    }
}

async fn fetch_genbank_filename(client: &Client, accession: &str) -> Result<(String, String)> {
    let (db, acc) = accession
        .trim()
        .split_once('_')
        .ok_or_else(|| anyhow!("Invalid accession format"))?;
    let (number, _) = acc.split_once('.').unwrap_or((acc, "1"));
    let number_path = number
        .chars()
        .collect::<Vec<_>>()
        .chunks(3)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("/");

    let base_url = format!(
        "https://ftp.ncbi.nlm.nih.gov/genomes/all/{}/{}",
        db, number_path
    );
    eprintln!("getting directory_response for accession {}", accession);
    let directory_response = client.get(&base_url).send().await;
    eprintln!("got directory_response for accession {}", accession);

    match directory_response {
        Ok(response) => {
            eprintln!(
                "got directory_response {:?} for accession {}",
                response.status(),
                accession
            );
            if !response.status().is_success() {
                eprintln!(
                    "Failed to open genome directory: HTTP {}, {}",
                    response.status(),
                    response
                        .status()
                        .canonical_reason()
                        .unwrap_or("Unknown reason")
                );
                return Err(anyhow!(
                    "Failed to open genome directory: HTTP {}, {}",
                    response.status(),
                    response
                        .status()
                        .canonical_reason()
                        .unwrap_or("Unknown reason")
                ));
            } else {
                eprintln!(
                    "Successfully opened genome directory: HTTP {}, {}",
                    response.status(),
                    response
                        .status()
                        .canonical_reason()
                        .unwrap_or("Successful operation")
                );
            }

            let text = response.text().await?;
            let link_regex = Regex::new(r#"<a href="([^"]*)""#)?;

            for cap in link_regex.captures_iter(&text) {
                let name = &cap[1];
                let clean_name = if name.ends_with('/') {
                    name.strip_suffix('/').unwrap()
                } else {
                    name
                };

                if clean_name.starts_with(db)
                    && clean_name
                        .split('_')
                        .nth(1)
                        .map_or(false, |x| x.starts_with(number))
                {
                    return Ok((format!("{}/{}", base_url, clean_name), clean_name.into()));
                }
            }
            Err(anyhow!(
                "No matching genome found for accession {}",
                accession
            ))
        }
        Err(e) => {
            eprintln!("HTTP request failed for accession {}: {}", accession, e);
            Err(anyhow!(
                "HTTP request failed for accession {}: {}",
                accession,
                e
            ))
        }
    }
}

// download and return data directly instead of saving to file
async fn download_with_retry(client: &Client, url: &str, retry_count: u32) -> Result<Vec<u8>> {
    let mut attempts = retry_count;
    while attempts > 0 {
        let response = client.get(url).send().await;
        match response {
            Ok(resp) if resp.status().is_success() => {
                let data = resp
                    .bytes()
                    .await
                    .context("Failed to read bytes from response")?;
                return Ok(data.to_vec()); // Return the downloaded data as Vec<u8>
            }
            _ => {
                attempts -= 1;
            }
        }
    }

    Err(anyhow!(
        "Failed to download file after {} retries: {}",
        retry_count,
        url
    ))
}

async fn sketch_data(
    name: &str,
    filename: &str,
    compressed_data: Vec<u8>,
    mut sigs: Vec<Signature>,
    moltype: &str,
) -> Result<Vec<Signature>> {
    task::block_in_place(|| {
        let cursor = Cursor::new(compressed_data);

        let mut fastx_reader =
            parse_fastx_reader(cursor).context("Failed to parse FASTA/FASTQ data")?;

        // for each sig in template list, add sequence to sketch
        let mut set_name = false;
        while let Some(record) = fastx_reader.next() {
            let record = record.context("Failed to read record")?;
            sigs.iter_mut().for_each(|sig| {
                if !set_name {
                    sig.set_name(name);
                    sig.set_filename(filename);
                };
                if moltype == "protein" {
                    sig.add_protein(&record.seq())
                        .expect("Failed to add protein");
                } else {
                    sig.add_sequence(&record.seq(), true)
                        .expect("Failed to add sequence");
                    // if not force, panics with 'N' in dna sequence
                }
            });
            if !set_name {
                set_name = true;
            }
        }

        Ok(sigs)
    })
}

pub struct FailedDownload {
    accession: String,
    name: String,
    url: String,
    moltype: String,
}

#[allow(clippy::too_many_arguments)]
async fn dl_sketch_accession(
    client: &Client,
    accession: String,
    name: String,
    location: &PathBuf,
    retry: Option<u32>,
    keep_fastas: bool,
    dna_sigs: Vec<Signature>,
    prot_sigs: Vec<Signature>,
    genomes_only: bool,
    proteomes_only: bool,
    download_only: bool,
) -> Result<(Vec<Signature>, Vec<FailedDownload>)> {
    let retry_count = retry.unwrap_or(3); // Default retry count
    let mut sigs = Vec::<Signature>::new();
    let mut failed = Vec::<FailedDownload>::new();

    // keep track of any accessions for which we fail to find URLs
    let (base_url, full_name) = match fetch_genbank_filename(client, accession.as_str()).await {
        Ok(result) => result,
        Err(_err) => {
            eprintln!("download error for acc {}", accession);
            // Add accession to failed downloads with each moltype
            if !proteomes_only {
                let failed_download_dna = FailedDownload {
                    accession: accession.clone(),
                    name: name.clone(),
                    url: "".to_string(),
                    moltype: "dna".to_string(),
                };
                failed.push(failed_download_dna);
            }
            if !genomes_only {
                let failed_download_protein = FailedDownload {
                    accession: accession.clone(),
                    name: name.clone(),
                    url: "".to_string(),
                    moltype: "protein".to_string(),
                };
                failed.push(failed_download_protein);
            }
            eprintln!(
                "dl+sketched {} sigs for accession {}!",
                sigs.len(),
                accession
            );

            return Ok((sigs, failed));
        }
    };

    let mut file_types = vec![
        GenBankFileType::Genomic,
        GenBankFileType::Protein,
        // GenBankFileType::AssemblyReport,
        // GenBankFileType::Checksum, // Including standalone files like checksums here
    ];
    if genomes_only {
        file_types = vec![GenBankFileType::Genomic];
    } else if proteomes_only {
        file_types = vec![GenBankFileType::Protein];
    }

    for file_type in &file_types {
        let url = file_type.url(&base_url, &full_name);
        let data = match download_with_retry(client, &url, retry_count).await {
            Ok(data) => data,
            Err(_err) => {
                // here --> keep track of accession errors + filetype
                let failed_download = FailedDownload {
                    accession: accession.clone(),
                    name: name.clone(),
                    url: url.clone(),
                    moltype: file_type.moltype(),
                };
                failed.push(failed_download);
                continue;
            }
        };
        let file_name = file_type.filename(&accession);

        if keep_fastas {
            let path = location.join(&file_name);
            fs::write(&path, &data).context("Failed to write data to file")?;
        }
        if !download_only {
            // sketch data
            match file_type {
                GenBankFileType::Genomic => sigs.extend(
                    sketch_data(
                        name.as_str(),
                        file_name.as_str(),
                        data,
                        dna_sigs.clone(),
                        "dna",
                    )
                    .await?,
                ),
                GenBankFileType::Protein => {
                    sigs.extend(
                        sketch_data(
                            name.as_str(),
                            file_name.as_str(),
                            data,
                            prot_sigs.clone(),
                            "protein",
                        )
                        .await?,
                    );
                }
                _ => {} // Do nothing for other file types
            };
        }
    }

    Ok((sigs, failed))
}

async fn write_sig(
    sig: &Signature,
    md5sum_occurrences: &mut HashMap<String, usize>,
    manifest_rows: &mut Vec<Record>,
    // zip_writer: &mut ZipFileWriter<&mut File>,
    // zip_writer: &mut ZipFileWriter<Compat<&mut tokio::fs::File>>,
    zip_writer: &mut ZipFileWriter<Compat<tokio::fs::File>>,
) -> Result<()> {
    let md5sum_str = sig.md5sum();
    let count = md5sum_occurrences.entry(md5sum_str.clone()).or_insert(0);
    *count += 1;

    let sig_filename = if *count > 1 {
        format!("signatures/{}_{}.sig.gz", md5sum_str, count)
    } else {
        format!("signatures/{}.sig.gz", md5sum_str)
    };

    let records: Vec<Record> = Record::from_sig(sig, &sig_filename);
    manifest_rows.extend(records);

    let wrapped_sig = vec![sig.clone()];
    let json_bytes = serde_json::to_vec(&wrapped_sig)
        .map_err(|e| anyhow!("Error serializing signature: {}", e))?;

    let gzipped_buffer = {
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut gz_writer = niffler::get_writer(
                Box::new(&mut buffer),
                niffler::compression::Format::Gzip,
                niffler::compression::Level::Nine,
            )?;
            //     .map_err(|e| anyhow!("Error creating gzip writer: {}", e))?;
            gz_writer.write_all(&json_bytes)?;
            //         .map_err(|e| anyhow!("Error writing gzip data: {}", e))?;
        }

        buffer.into_inner()
    };

    let now = Utc::now();
    let builder = ZipEntryBuilder::new(sig_filename.into(), Compression::Stored)
        .last_modification_date(ZipDateTime::from_chrono(&now));
    zip_writer
        .write_entry_whole(builder, &gzipped_buffer)
        .await
        .map_err(|e| anyhow!("Error writing zip entry: {}", e))
}

pub fn sigwriter_handle(
    mut recv_sigs: tokio::sync::mpsc::Receiver<Vec<Signature>>,
    output_sigs: String,
    mut error_sender: tokio::sync::mpsc::Sender<anyhow::Error>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut md5sum_occurrences = HashMap::new();
        let mut manifest_rows = Vec::new();
        let mut wrote_sigs = false;
        let outpath: PathBuf = output_sigs.into();

        let file = match File::create(&outpath).await {
            Ok(file) => file,
            Err(e) => {
                let error =
                    anyhow::Error::new(e).context("Failed to create file at specified path");
                let _ = error_sender.send(error).await; // Send the error through the channel
                return; // Simply exit the task as error handling is managed elsewhere
            }
        };
        let mut zip_writer = ZipFileWriter::with_tokio(file);

        while let Some(sigs) = recv_sigs.recv().await {
            for sig in sigs {
                match write_sig(
                    &sig,
                    &mut md5sum_occurrences,
                    &mut manifest_rows,
                    &mut zip_writer,
                )
                .await
                {
                    Ok(_) => wrote_sigs = true,
                    Err(e) => {
                        let error = e.context("Error processing signature");
                        if let Err(send_error) = error_sender.send(error).await {
                            eprintln!("Error sending to error channel: {}", send_error);
                            return; // Exit on failure to send error
                        }
                    }
                }
            }
        }

        if wrote_sigs {
            println!("Writing manifest");
            let manifest_filename = "SOURMASH-MANIFEST.csv".to_string();
            let manifest: Manifest = manifest_rows.clone().into();
            let mut manifest_buffer = Vec::new();
            manifest
                .to_writer(&mut manifest_buffer)
                .expect("Failed to serialize manifest"); // Handle this more gracefully in production

            let now = Utc::now();
            let builder = ZipEntryBuilder::new(manifest_filename.into(), Compression::Stored)
                .last_modification_date(ZipDateTime::from_chrono(&now));

            if let Err(e) = zip_writer
                .write_entry_whole(builder, &manifest_buffer)
                .await
            {
                let error = anyhow::Error::new(e).context("Failed to write manifest to ZIP");
                let _ = error_sender.send(error).await;
                return;
            }

            if let Err(e) = zip_writer.close().await {
                let error = anyhow::Error::new(e).context("Failed to close ZIP file");
                let _ = error_sender.send(error).await;
                return;
            }
        } else {
            let error = anyhow::Error::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "No signatures were processed",
            ));
            let _ = error_sender.send(error).await; // Send error about no signatures processed
        }
    })
}

pub fn failures_handle(
    failed_csv: String,
    mut recv_failed: tokio::sync::mpsc::Receiver<FailedDownload>,
    mut error_sender: tokio::sync::mpsc::Sender<Error>, // Additional parameter for error channel
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match File::create(&failed_csv).await {
            Ok(file) => {
                let mut writer = BufWriter::new(file);

                // Attempt to write CSV headers
                if let Err(e) = writer.write_all(b"accession,name,moltype,url\n").await {
                    let error = Error::new(e).context("Failed to write headers");
                    let _ = error_sender.send(error).await;
                    return; // Exit the task early after reporting the error
                }

                while let Some(FailedDownload {
                    accession,
                    name,
                    moltype,
                    url,
                }) = recv_failed.recv().await
                {
                    let record = format!("{},{},{},{}\n", accession, name, moltype, url);

                    // Attempt to write each record
                    if let Err(e) = writer.write_all(record.as_bytes()).await {
                        let error = Error::new(e).context("Failed to write record");
                        let _ = error_sender.send(error).await;
                        continue; // Optionally continue to try to write next records
                    }
                }

                // Attempt to flush the writer
                if let Err(e) = writer.flush().await {
                    let error = Error::new(e).context("Failed to flush writer");
                    let _ = error_sender.send(error).await;
                }
            }
            Err(e) => {
                let error = Error::new(e).context("Failed to create file");
                let _ = error_sender.send(error).await;
            }
        }
    })
}

#[tokio::main]
#[allow(clippy::too_many_arguments)]
pub async fn download_and_sketch(
    py: Python,
    input_csv: String,
    output_sigs: String,
    param_str: String,
    failed_csv: String,
    retry_times: u32,
    fasta_location: String,
    keep_fastas: bool,
    genomes_only: bool,
    proteomes_only: bool,
    download_only: bool,
) -> Result<(), anyhow::Error> {
    // if sig output doesn't end in zip, bail
    if Path::new(&output_sigs)
        .extension()
        .map_or(true, |ext| ext != "zip")
    {
        bail!("Output must be a zip file.");
    }

    // set up fasta download path
    let download_path = PathBuf::from(fasta_location);
    if !download_path.exists() {
        create_dir_all(&download_path)?;
    }

    // create channels. buffer size can be changed - here it is 4 b/c we can do 3 downloads simultaneously
    // to do: see whether increasing buffer size speeds things up
    let (send_sigs, recv_sigs) = tokio::sync::mpsc::channel::<Vec<Signature>>(4);
    let (send_failed, recv_failed) = tokio::sync::mpsc::channel::<FailedDownload>(4);
    // Error channel for handling task errors
    let (error_sender, mut error_receiver) = tokio::sync::mpsc::channel::<anyhow::Error>(1);

    //  // Set up collector/writing tasks
    let mut handles = Vec::new();
    let sig_handle = sigwriter_handle(recv_sigs, output_sigs, error_sender.clone());
    let failures_handle = failures_handle(failed_csv, recv_failed, error_sender);
    handles.push(sig_handle);
    handles.push(failures_handle);
    // set up error handling
    while let Some(error) = error_receiver.recv().await {
        eprintln!("Error occurred: {}", error);
    }

    // Worker tasks
    // let client = Client::new();
    let semaphore = Arc::new(Semaphore::new(3)); // Limiting concurrent downloads
    let client = Arc::new(Client::new());
    // let semaphore = Arc::new(Semaphore::new(3)); // Allows up to 3 concurrent tasks
    let mut interval = tokio::time::interval(Duration::from_secs(1));

    // Open the file containing the accessions synchronously
    let (accession_info, n_accs) = load_accession_info(input_csv)?;
    if n_accs == 0 {
        bail!("No accessions to download and sketch.")
    }

    // parse param string into params_vec, print error if fail
    let param_result = parse_params_str(param_str);
    let params_vec = match param_result {
        Ok(params) => params,
        Err(e) => {
            eprintln!("Error parsing params string: {}", e);
            bail!("Failed to parse params string");
        }
    };
    let dna_sig_templates = build_siginfo(&params_vec, "DNA");
    let prot_sig_templates = build_siginfo(&params_vec, "protein");

    // report every percent (or ever 1, whichever is larger)
    let reporting_threshold = std::cmp::max(n_accs / 100, 1);

    // for accinfo in accession_info {
    for (i, accinfo) in accession_info.into_iter().enumerate() {
        let semaphore_clone = Arc::clone(&semaphore);
        let client_clone = Arc::clone(&client);
        let send_sigs = send_sigs.clone();
        let send_failed = send_failed.clone();
        let download_path_clone = download_path.clone(); // Clone the path for each task

        let dna_sigs = dna_sig_templates.clone();
        let prot_sigs = prot_sig_templates.clone();

        // Check for interrupt periodically
        if i % 100 == 0 {
            py.check_signals()?; // If interrupted, return an Err automatically
        }
        interval.tick().await; // Wait for the next interval tick before continuing

        if (i + 1) % reporting_threshold == 0 {
            let percent_processed = (((i + 1) as f64 / n_accs as f64) * 100.0).round();
            println!(
                "Starting accession {}/{} ({}%)",
                (i + 1),
                n_accs,
                percent_processed
            );
        }

        tokio::spawn(async move {
            let _permit = semaphore_clone.acquire().await;
            // Perform download and sketch
            let result = dl_sketch_accession(
                &client_clone,
                accinfo.accession.clone(),
                accinfo.name.clone(),
                &download_path_clone,
                Some(retry_times),
                keep_fastas,
                dna_sigs,
                prot_sigs,
                genomes_only,
                proteomes_only,
                download_only,
            )
            .await;
            match result {
                Ok((sigs, failed_downloads)) => {
                    if let Err(e) = send_sigs.send(sigs).await {
                        eprintln!("Failed to send signatures: {}", e);
                    }
                    for fail in failed_downloads {
                        if let Err(e) = send_failed.send(fail).await {
                            eprintln!("Failed to send failed download info: {}", e);
                        }
                    }
                }
                Err(e) => eprintln!("Error during download and sketch: {}", e),
            }
        });
    }
    // Wait for all tasks to complete
    for handle in handles {
        if let Err(e) = handle.await {
            eprintln!("A task encountered an error: {}", e);
        }
    }

    // Handle errors received from the error channel
    while let Some(error) = error_receiver.recv().await {
        eprintln!("Error occurred: {}", error);
    }
    Ok(())
}
