extern crate reqwest;
extern crate rayon;
extern crate json;
extern crate sha2;
extern crate ini;
extern crate hex;

//Standard library
use std::collections::HashMap;
use std::io::{Read, Write, Seek, SeekFrom};
use std::fs::{OpenOptions,DirBuilder};
use std::sync::{Arc, Mutex};
use std::panic;

//Modules
mod mirrors;
mod traits;
use mirrors::Mirrors;
use traits::{AsString, BorrowUnwrap};

//External crates
use rayon::prelude::*;
use ini::Ini;
use sha2::{Sha256, Digest};

pub struct Progress {
  pub download_size: (u64,u64), //Downloaded .. out of .. bytes
  patch_files: (u64, u64), //Patched .. out of .. files
  pub finished_hash: bool,
  pub finished_patching: bool,
}

impl Progress {
  fn new() -> Progress {
    Progress {
      download_size: (0,0),
      patch_files: (0,0),
      finished_hash: false,
      finished_patching: false,
    }
  }
}

#[derive(Debug)]
struct Instruction {
  path: String,
  old_hash: Option<String>,
  new_hash: Option<String>,
  compressed_hash: Option<String>,
  delta_hash: Option<String>,
  full_replace_size: usize,
  delta_size: usize,
  has_delta: bool
}

#[derive(Debug)]
pub struct PatchEntry {
  target_path: String,
  delta_path: String,
  has_source: bool,
  target_hash: String,
}

#[derive(Debug)]
pub struct DownloadEntry {
  file_path: String,
  file_size: usize,
  file_hash: String,
  patch_entries: Vec<PatchEntry>,
}

pub struct Downloader {
  renegadex_location: Option<String>, //Os dependant
  mirrors: Mirrors,
  instructions: Vec<Instruction>, //instructions.json
  pub state: Arc<Mutex<Progress>>,
  download_hashmap: Mutex<HashMap<String, DownloadEntry>>,
  hash_queue: Mutex<Vec<Instruction>>,
}


impl Downloader {
  pub fn new() -> Downloader {
    Downloader {
      renegadex_location: None,
      mirrors: Mirrors::new(),
      instructions: Vec::new(),
      state: Arc::new(Mutex::new(Progress::new())),
      download_hashmap: Mutex::new(HashMap::new()),
      hash_queue: Mutex::new(Vec::new()),
    }
  }
  pub fn set_location(&mut self, loc: String) {
    self.renegadex_location = Some(format!("{}/", loc).replace("\\","/").replace("//","/"));
  }
  
  pub fn retrieve_mirrors(&mut self, location: &String) {
    self.mirrors.get_mirrors(location);
  }

  pub fn update_available(&self) -> bool {
    if self.mirrors.is_empty() {
      panic!("No mirrors found, aborting! Did you retrieve mirrors?");
    }
    if self.renegadex_location.is_none() {
      panic!("The RenegadeX location hasn't been set, aborting!");
    }
    let patch_dir_path = format!("{}/patcher/", self.renegadex_location.borrow()).replace("//", "/");
    match std::fs::read_dir(patch_dir_path) {
      Ok(iter) => {
        if iter.count() != 0 {
          return true
        }
      },
      Err(_e) => {}
    };

    let path = format!("{}UDKGame/Config/DefaultRenegadeX.ini", self.renegadex_location.borrow());
    let conf = match Ini::load_from_file(&path) {
      Ok(file) => file,
      Err(_e) => { return true }
    };

    let section = conf.section(Some("RenX_Game.Rx_Game".to_owned())).unwrap();
    let game_version_number = section.get("GameVersionNumber").unwrap();

    if self.mirrors.version_number.borrow() != game_version_number {
      return true;
    }
    return false;
  }

  pub fn download(&mut self) {
    if self.mirrors.is_empty() {
      panic!("No mirrors found! Did you retrieve mirrors?");
    }
    if self.instructions.len() == 0 {
      self.retrieve_instructions();
    }
    println!("Retrieved instructions, checking hashes.");
    self.check_hashes();
    self.download_files();
    {
      let state = self.state.lock().unwrap();
      println!("{:#?}", &state.download_size);
    }
  }
  
  /*
   * Downloads instructions.json from a mirror, checks its validity and passes it on to process_instructions()
   * -------------------------      ------------  par   ------------------------
   * | retrieve_instructions |  --> | Get Json | ---->  | process_instructions | 
   * -------------------------      ------------        ------------------------
  */
  fn retrieve_instructions(&mut self) {
    if self.mirrors.is_empty() {
      panic!("No mirrors found! Did you retrieve mirrors?");
    }
    let instructions_mutex : Mutex<String> = Mutex::new("".to_string());
    for retry in 0..3 {
      let result = std::panic::catch_unwind(|| {
        let instructions_url = format!("{}/instructions.json", &self.mirrors.mirrors[retry].address);
        println!("{}", &instructions_url);
        let mut instructions_response = match reqwest::get(&instructions_url) {
          Ok(result) => result,
          Err(e) => panic!("Is your internet down? {}", e)
        };
        let text = instructions_response.text().unwrap();
        // check instructions hash
        let mut sha256 = Sha256::new();
        sha256.input(&text);
        let hash = hex::encode_upper(sha256.result());
        if &hash != self.mirrors.instructions_hash.borrow() {
          panic!("Hashes did not match!");
        }
        *instructions_mutex.lock().unwrap() = text;
      });
      if result.is_ok() {
        for _i in 0..retry {
          println!("Removing mirror: {:#?}", &self.mirrors.mirrors[0]);
          self.mirrors.remove(0);
        }
        break;
      } else if result.is_err() && retry == 2 {
        panic!("Couldn't fetch instructions.json");
      }
    }
    let instructions_text : String = instructions_mutex.into_inner().unwrap();
    let instructions_data = match json::parse(&instructions_text) {
      Ok(result) => result,
      Err(e) => panic!("Invalid JSON: {}", e)
    };
    self.process_instructions(instructions_data);
  }

  /*
   * ------------------------   par   --------------------
   * | process_instructions |  ---->  | Try to Open File | 
   * ------------------------         --------------------
   *                                   |                |
   *                                   |                |
   *                           ------------------    ----------
   *                           | Err(Not Found) |    | Ok(()) |
   *                           ------------------    ----------
   *                                 |                   |
   *                                 |                   |
   *                    ------------------------   --------------------
   *                    | Add to DownloadQueue |   | Add to HashQueue |
   *                    | Add size to size sum |   --------------------
   *                    | Add to Patch HashMap |
   *                    ------------------------
   * 
   */
  fn process_instructions(&self, instructions_data: json::JsonValue) {
    instructions_data.into_inner().par_iter().for_each(|instruction| {
      //lets start off by trying to open the file.
      let file_path = format!("{}{}", self.renegadex_location.borrow(), instruction["Path"].as_string().replace("\\", "/"));
      match OpenOptions::new().read(true).open(&file_path) {
        Ok(_file) => {
          if !instruction["NewHash"].is_null() {
            let mut hash_queue = self.hash_queue.lock().unwrap();
            let hash_entry = Instruction {
              path:                file_path,
              old_hash:            instruction["OldHash"].as_string_option(),
              new_hash:            instruction["NewHash"].as_string_option(),
              compressed_hash:     instruction["CompressedHash"].as_string_option(),
              delta_hash:          instruction["DeltaHash"].as_string_option(),
              full_replace_size:   instruction["FullReplaceSize"].as_usize().unwrap(),
              delta_size:          instruction["DeltaSize"].as_usize().unwrap(),
              has_delta:           instruction["HasDelta"].as_bool().unwrap()
            };
            hash_queue.push(hash_entry);
          } else {
            //TODO: DeletionQueue, delete it straight away?
          }
        },
        Err(_e) => {
          if !instruction["NewHash"].is_null() {
            let key = instruction["NewHash"].as_string();
            let delta_path = format!("{}patcher/{}", self.renegadex_location.borrow(), &key);
            let mut download_hashmap = self.download_hashmap.lock().unwrap();
            if !download_hashmap.contains_key(&key) {
              let download_entry = DownloadEntry {
                file_path: delta_path.clone(),
                file_size: instruction["FullReplaceSize"].as_usize().unwrap(),
                file_hash: instruction["CompressedHash"].as_string(),
                patch_entries: Vec::new(),
              };
              download_hashmap.insert(key.clone(), download_entry);
              let mut state = self.state.lock().unwrap();
              state.download_size.1 += instruction["FullReplaceSize"].as_u64().unwrap();
            }
            let patch_entry = PatchEntry {
              target_path: file_path,
              delta_path: delta_path,
              has_source: false,
              target_hash: key.clone(),
            };
            download_hashmap.get_mut(&key).unwrap().patch_entries.push(patch_entry); //should we add it to a downloadQueue??
          }
        }
      };
    });
  }

/*
 * -------------  par ----------------------     -----------------------
 * | HashQueue |  --> | Check Hash of File | --> | Compare to OldDelta | 
 * -------------      ----------------------     -----------------------
 *                                                 |                |
 *                                                 |                |
 *                                         -------------       ----------
 *                                         | Different |       |  Same  |
 *                                         -------------       ----------
 *                                              |                   |
 *                                              |                   |
 *                         ----------------------------------   ------------------------------
 *                         | Add Full File to DownloadQueue |   | Add Delta to DownloadQueue |
 *                         |      Add size to size sum      |   |    Add size to size sum    |
 *                         |      Add to Patch HashMap      |   |    Add to Patch Hashmap    |
 *                         ----------------------------------   ------------------------------
 */
  fn check_hashes(&mut self) {
    let hash_queue = self.hash_queue.lock().unwrap();
    hash_queue.par_iter().for_each(|hash_entry| {
      let file_hash = self.get_hash(&hash_entry.path);
      if hash_entry.old_hash.is_some() && hash_entry.new_hash.is_some() && &file_hash == hash_entry.old_hash.borrow() && &file_hash != hash_entry.new_hash.borrow() {
        //download patch file
        let key = format!("{}_from_{}", hash_entry.new_hash.borrow(), hash_entry.old_hash.borrow());
        let delta_path = format!("{}patcher/{}", self.renegadex_location.borrow(), &key);
        let mut download_hashmap = self.download_hashmap.lock().unwrap();
        if !download_hashmap.contains_key(&key) {
          let download_entry = DownloadEntry {
            file_path: delta_path.clone(),
            file_size: hash_entry.delta_size,
            file_hash: hash_entry.delta_hash.clone().unwrap(),
            patch_entries: Vec::new(),
          };
          download_hashmap.insert(key.clone(), download_entry);
          let mut state = self.state.lock().unwrap();
          state.download_size.1 += hash_entry.delta_size as u64;
        }

        let patch_entry = PatchEntry {
          target_path: hash_entry.path.clone(),
          delta_path: delta_path,
          has_source: true,
          target_hash: key.clone(),
        };
        download_hashmap.get_mut(&key).unwrap().patch_entries.push(patch_entry);
      } else if hash_entry.new_hash.is_some() && &file_hash == hash_entry.new_hash.borrow() {
        //this file is up to date
      } else {
        //this file does not math old hash, nor the new hash, thus it's corrupted
        //download full file
        println!("File {} is corrupted!", &hash_entry.path);
        let key : &String = hash_entry.new_hash.borrow();
        let delta_path = format!("{}patcher/{}", self.renegadex_location.borrow(), &key);
        let mut download_hashmap = self.download_hashmap.lock().unwrap();
        if !download_hashmap.contains_key(key) {
         let download_entry = DownloadEntry {
            file_path: delta_path.clone(),
            file_size: hash_entry.full_replace_size,
            file_hash: hash_entry.compressed_hash.clone().unwrap(),
            patch_entries: Vec::new(),
          };
          download_hashmap.insert(key.clone(), download_entry);
          let mut state = self.state.lock().unwrap();
          state.download_size.1 += hash_entry.full_replace_size as u64;
        }

        let patch_entry = PatchEntry {
          target_path: hash_entry.path.clone(),
          delta_path: delta_path,
          has_source: false,
          target_hash: key.clone(),
        };
        download_hashmap.get_mut(key).unwrap().patch_entries.push(patch_entry);
      }
    });
    self.state.lock().unwrap().finished_hash = true;
  }


/*
 * Iterates over the hash_queue and downloads the files
 */
  fn download_files(&self) {
    let download_hashmap = self.download_hashmap.lock().unwrap();
    download_hashmap.par_iter().for_each(|(key, download_entry)| {
      for attempt in 0..5 {
        let download_url = match download_entry.patch_entries[0].has_source {
          true => format!("{}/delta/{}", self.mirrors.mirrors[attempt].address, &key),
          false => format!("{}/full/{}", self.mirrors.mirrors[attempt].address, &key)
        };
        
        match self.download_file(download_url, download_entry) {
          Ok(()) => break,
          Err(_e) => {
            if attempt == 4 { panic!("Couldn't download file: {}", &key) }
          },
        };
      }
      //apply delta
      download_entry.patch_entries.par_iter().for_each(|patch_entry| {
        self.apply_patch(patch_entry);
      });
      std::fs::remove_file(&download_entry.file_path).unwrap();
    });
    {
      let mut state = self.state.lock().unwrap();
      state.finished_patching = true;
    }
    //remove patcher folder and all remaining files in there:
    std::fs::remove_dir_all(format!("{}patcher/", &self.renegadex_location.borrow())).unwrap();
  }


/*
 * Iterates over the hash_queue and downloads the files
 */
  fn download_file(&self, download_url: String, download_entry: &DownloadEntry) -> Result<(), &'static str> {
    //println!("{}", download_url);
    //println!("{:#?}", &download_entry);

    let part_size = 10u64.pow(6) as usize; //1.000.000
    let mut f = OpenOptions::new().read(true).write(true).create(true).open(&download_entry.file_path).unwrap();
    //set the size of the file, add a 32bit integer to the end of the file as a means of tracking progress. We won't download parts async.
    let parts_amount : usize = download_entry.file_size / part_size + if download_entry.file_size % part_size > 0 {1} else {0};
    let file_size : usize = download_entry.file_size + 4;
    if (f.metadata().unwrap().len() as usize) < file_size {
      if f.metadata().unwrap().len() == (download_entry.file_size as u64) {
        //If hash is correct, return.
        //Otherwise download again.
        let hash = self.get_hash(&download_entry.file_path);
        if &hash == &download_entry.file_hash {
          let mut state = self.state.lock().unwrap();
          state.download_size.0 += (download_entry.file_size) as u64;
          return Ok(());
        }
      }
      match f.set_len(file_size as u64) {
        Ok(()) => println!("Succesfully set file size"),
        Err(e) => {
          println!("Couldn't set file size! {}", e);
          return Err("Could not change file size of patch file, is it in use?");
        }
      }
    }
    let http_client = reqwest::Client::new();
    f.seek(SeekFrom::Start((download_entry.file_size) as u64)).unwrap();
    let mut buf = [0,0,0,0];
    f.read_exact(&mut buf).unwrap();
    let resume_part : usize = u32::from_be_bytes(buf) as usize;
    if resume_part != 0 { 
      println!("Resuming download from part: {}", resume_part);
      let mut state = self.state.lock().unwrap();
      state.download_size.0 += (part_size * resume_part) as u64;
    };
    //iterate over all parts, downloading them into memory, writing them into the file, adding one to the counter at the end of the file.
    for part_int in resume_part..parts_amount {
      let bytes_start = part_int * part_size;
      let mut bytes_end = part_int * part_size + part_size -1;
      if bytes_end > download_entry.file_size {
        bytes_end = download_entry.file_size.clone();
      }
      let download_request = http_client.get(&download_url).header(reqwest::header::RANGE,format!("bytes={}-{}", bytes_start, bytes_end));
      let download_response = download_request.send();
      f.seek(SeekFrom::Start(bytes_start as u64)).unwrap();
      let mut content : Vec<u8> = Vec::with_capacity(bytes_end - bytes_start + 1);
      download_response.unwrap().read_to_end(&mut content).unwrap();
      f.write_all(&content).unwrap();
      //completed downloading and writing this part, so update the progress-tracker at the end of the file
      f.seek(SeekFrom::Start((download_entry.file_size) as u64)).unwrap();
      f.write_all(&(part_int as u32).to_be_bytes()).unwrap();
      let mut state = self.state.lock().unwrap();
      state.download_size.0 += (bytes_end - bytes_start) as u64;
    }
    //Remove the counter at the end of the file to finish the vcdiff file
    f.set_len(download_entry.file_size as u64).unwrap();
    
    //Let's make sure the downloaded file matches the Hash found in Instructions.json
    let hash = self.get_hash(&download_entry.file_path);
    if &hash != &download_entry.file_hash {
      println!("Hash is incorrect!");
      println!("{} vs {}", &hash, &download_entry.file_hash);
      return Err("Downloaded file's hash did not match with the one provided in Instructions.json");
    }
    return Ok(());
  }


/*
 * Applies the vcdiff patch file to the target file.
 * 
 * -------------- par --------------------------------------------------
 * | DeltaQueue | --> | apply patch to all files that match this Delta |
 * --------------     --------------------------------------------------
 */
  fn apply_patch(&self, patch_entry: &PatchEntry) {
    let mut dir_path = patch_entry.target_path.clone();
    dir_path.truncate(patch_entry.target_path.rfind('/').unwrap());
    DirBuilder::new().recursive(true).create(dir_path).unwrap();
    if patch_entry.has_source {
      let source_path = format!("{}.vcdiff_src", &patch_entry.target_path);
      std::fs::rename(&patch_entry.target_path, &source_path).unwrap();
      xdelta::decode_file(Some(&source_path), &patch_entry.delta_path, &patch_entry.target_path);
      std::fs::remove_file(&source_path).unwrap();
    } else {
      //there is supposed to be no source file, so make sure it doesn't exist either!
      match std::fs::remove_file(&patch_entry.target_path) {
        Ok(()) => (),
        Err(_e) => ()
      };
      xdelta::decode_file(None, &patch_entry.delta_path, &patch_entry.target_path);
    }
    let hash = self.get_hash(&patch_entry.target_path);
    if &hash != &patch_entry.target_hash {
      panic!("Hash for file {} is incorrect!", &patch_entry.target_path);
    }
  }


/*
 * Opens a file and calculates it's SHA256 hash
 */
  fn get_hash(&self, file_path: &String) -> String {
    let mut file = OpenOptions::new().read(true).open(file_path).unwrap();
    let mut sha256 = Sha256::new();
    std::io::copy(&mut file, &mut sha256).unwrap();
    hex::encode_upper(sha256.result())
  }
  
/*
 * Spawns magical unicorns
 */
  pub fn poll_progress(&self) {
    let state = self.state.clone();
    std::thread::spawn(move || {
      let mut finished_hash = false;
      let mut finished_patching = false;
      let start_time = std::time::Instant::now();
      let mut old_time = std::time::Instant::now();
      let mut old_download_size : (u64, u64) = (0, 0);
      while !finished_patching {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let mut download_size : (u64, u64) = (0, 0);
        {
          let state = state.lock().unwrap();
          finished_hash = state.finished_hash.clone();
          finished_patching = state.finished_patching.clone();
          download_size = state.download_size.clone();
        }
        if old_download_size != download_size {
          let elapsed = old_time.elapsed();
          old_time = std::time::Instant::now();
          println!("Downloaded {:.3}/{:.3} MB, speed: {:.3} MB/s", (download_size.0 as f64)*0.000001, (download_size.1 as f64)*0.000001, ((download_size.0 - old_download_size.0) as f64)/(elapsed.as_micros() as f64));
          old_download_size = download_size;
        }
      }
    });
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn downloader() {
    let mut patcher : Downloader = Downloader::new();
    patcher.set_location("/home/sonny/RenegadeX/game_files/".to_string());
    patcher.retrieve_mirrors(&"https://static.renegade-x.com/launcher_data/version/release.json".to_string());
    if patcher.update_available() {
      println!("Update available!");
      patcher.poll_progress();
      patcher.download();
    };
    assert!(true);
  }
}
