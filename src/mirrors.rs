use std::time::{Duration, Instant};

use crate::traits::{AsString,Error};
use std::sync::Arc;
use std::net::ToSocketAddrs;

#[derive(Debug, Clone)]
pub struct Mirror {
  pub address: Arc<String>,
  pub speed: f64,
  pub ping: f64,
  pub in_use: usize,
  pub enabled: bool,
  pub ip: SocketAddrs,//Vec<std::net::SocketAddr>,
}

#[derive(Debug, Clone)]
pub struct SocketAddrs {
  inner: Vec<std::net::SocketAddr>
}

impl From<url::SocketAddrs> for SocketAddrs {
  fn from(other: url::SocketAddrs) -> Self {
    SocketAddrs {
      inner: other.collect()
    }
  }
}

impl ToSocketAddrs for SocketAddrs {
  type Iter = std::vec::IntoIter<std::net::SocketAddr>;
  fn to_socket_addrs(&self) -> std::io::Result<std::vec::IntoIter<std::net::SocketAddr>> {
    Ok(self.inner.clone().into_iter())
  }
}

pub struct Mirrors {
  pub mirrors: Vec<Mirror>,
  pub instructions_hash: Option<String>,
  pub version_number: Option<String>,
}

impl Mirrors {
  pub fn new() -> Mirrors {
    Mirrors {
      mirrors: Vec::new(),
      instructions_hash: None,
      version_number: None,
    }
  }

  pub fn is_empty(&self) -> bool {
    if self.mirrors.len() == 0 {
      true
    } else {
      false
    }
  }

  pub fn remove(&mut self, entry: usize) {
    self.mirrors.remove(entry);
  }

  pub fn disable(&mut self, entry: usize) {
    self.mirrors[entry].enabled = false;
  }

  /**
  Downloads release.json from the renegade-x server and adds it to the struct
  */
  pub fn get_mirrors(&mut self, location: &String) -> Result<(), Error> {
    let mut release_json = match reqwest::get(location) {
      Ok(result) => result,
      Err(e) => return Err(format!("Is your internet down? {}", e).into())
    };
    let release_json_response = match release_json.text() {
      Ok(result) => result,
      Err(e) => return Err(format!("mirrors.rs: Corrupted response: {}", e).into())
    };
    let release_data = match json::parse(&release_json_response) {
      Ok(result) => result,
      Err(e) => return Err(format!("mirrors.rs: Invalid JSON: {}", e).into())
    };
    let mut mirror_vec = Vec::with_capacity(release_data["game"]["mirrors"].len());
    release_data["game"]["mirrors"].members().for_each(|mirror| mirror_vec.push(mirror["url"].as_string()) );
    for mirror in mirror_vec {
      self.mirrors.push(Mirror{
        address: Arc::new(format!("{}{}", &mirror, release_data["game"]["patch_path"].as_string())),
        ip: mirror.parse::<url::Url>().unwrap().to_socket_addrs().unwrap().into(),
        speed: 80.0,
        ping: 500.0,
        in_use: 0,
        enabled: false,
      });
    }
    self.test_mirrors()?;
    println!("{:#?}", &self.mirrors);
    self.instructions_hash = Some(release_data["game"]["instructions_hash"].as_string());
    self.version_number = Some(release_data["game"]["version_number"].as_u64().unwrap().to_string());
    return Ok(());
  }

  
  pub fn get_mirror(&self) -> Mirror {
    for i in 0..5 {
      for mirror in self.mirrors.iter() {
        if mirror.enabled && Arc::strong_count(&mirror.address) == i {
          return mirror.clone();
        }
      }
    }
    panic!("No mirrors found?");
  }

  /**
  Checks the speed on the mirrors again
  */
  pub fn test_mirrors(&mut self) -> Result<(), Error> {
    let mut handles = Vec::new();
    for i in 0..self.mirrors.len() {
      let mirror = self.mirrors[i].clone();
      let fastest_mirror_speed = self.mirrors[0].speed;
      handles.push(std::thread::spawn(move || -> Mirror {
        let mut url = format!("{}", mirror.address.to_owned());
        url.truncate(url.rfind("/").unwrap() + 1);
        let http_client = reqwest::Client::builder().timeout(Duration::from_millis(10000/fastest_mirror_speed as u64 * 4)).build().unwrap();
        url.push_str("10kb_file");
        let download_request = http_client.get(url.as_str());
        let start = Instant::now();
        let download_response = download_request.send();
        match download_response {
          Ok(result) => {
            let duration = start.elapsed();
            let content_length = result.headers().get("content-length");
            if content_length.is_none() {
              Mirror { 
                address: mirror.address,
                ip: mirror.ip,
                speed: 0.0,
                ping: 1000.0,
                in_use: mirror.in_use,
                enabled: false,
              }
            } else {
              if content_length.unwrap() != "10000" { 
                Mirror { 
                  address: mirror.address,
                  ip: mirror.ip,
                  speed: 0.0,
                  ping: 1000.0,
                  in_use: mirror.in_use,
                  enabled: false,
                }
              } else {
                Mirror { 
                  address: mirror.address,
                  ip: mirror.ip,
                  speed: (10000 as f64)/(duration.as_millis() as f64),
                  ping: (duration.as_micros() as f64)/(1000 as f64),
                  in_use: mirror.in_use,
                  enabled: true,
                }
              }
            }
          },
          Err(_e) => {
            Mirror { 
              address: mirror.address,
              ip: mirror.ip,
              speed: 0.0,
              ping: 1000.0,
              in_use: mirror.in_use,
              enabled: false,
            }
          }
        }
      }));
    }
    for handle in handles {
      match handle.join() {
        Ok(mirror) => {
          for i in 0..self.mirrors.len() {
            if self.mirrors[i].address == mirror.address {
              self.mirrors[i] = mirror;
              break;
            }
          }
        },
        Err(_) => { panic!("Failed to execute thread in test_mirrors!") }
      }
    }
    if self.mirrors.len() > 1 {
      self.mirrors.sort_by(|a,b| b.speed.partial_cmp(&a.speed).unwrap());
      let best_speed = self.mirrors[0].speed;
      for mut elem in self.mirrors.iter_mut() {
        if elem.speed < best_speed / 4.0 {
          elem.enabled = false;
        }
      }
    }
    return Ok(());
  }
}
