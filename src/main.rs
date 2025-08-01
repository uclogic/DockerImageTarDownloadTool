use std::env;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::Path;
use std::process;
use std::time::Duration;

use flate2::read::GzDecoder;
use reqwest::{Client, Proxy};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::fs::File as AsyncFile;
use tokio::io::AsyncWriteExt;

#[derive(Debug)]
struct DockerImage {
    registry: String,
    repository: String,
    tag: String,
    img: String,
    repo: String,
}

#[derive(Debug)]
struct Layer {
    digest: String,
    urls: Option<Vec<String>>,
}

async fn create_client() -> Result<Client, reqwest::Error> {
    let mut client_builder = Client::builder()
        .timeout(Duration::from_secs(30))
        .danger_accept_invalid_certs(true);

    // Check for proxy environment variables
    if let Ok(http_proxy) = env::var("HTTP_PROXY").or_else(|_| env::var("http_proxy")) {
        println!("[+] Using HTTP proxy: {}", http_proxy);
        client_builder = client_builder.proxy(Proxy::http(http_proxy)?);
    }

    if let Ok(https_proxy) = env::var("HTTPS_PROXY").or_else(|_| env::var("https_proxy")) {
        println!("[+] Using HTTPS proxy: {}", https_proxy);
        client_builder = client_builder.proxy(Proxy::https(https_proxy)?);
    }

    client_builder.build()
}

fn parse_image_name(image_name: &str) -> DockerImage {
    let mut repo = "library".to_string();
    let mut tag = "latest".to_string();

    let parts: Vec<&str> = image_name.split('/').collect();

    let (img, parsed_tag) = if let Some(last_part) = parts.last() {
        if last_part.contains('@') {
            let digest_parts: Vec<&str> = last_part.split('@').collect();
            (digest_parts[0].to_string(), digest_parts[1].to_string())
        } else if last_part.contains(':') {
            let tag_parts: Vec<&str> = last_part.split(':').collect();
            (tag_parts[0].to_string(), tag_parts[1].to_string())
        } else {
            (last_part.to_string(), tag.clone())
        }
    } else {
        (image_name.to_string(), tag.clone())
    };

    tag = parsed_tag;

    let registry = if parts.len() > 1 && (parts[0].contains('.') || parts[0].contains(':')) {
        repo = parts[1..parts.len() - 1].join("/");
        parts[0].to_string()
    } else {
        if parts.len() > 1 {
            repo = parts[..parts.len() - 1].join("/");
        }
        "registry-1.docker.io".to_string()
    };

    let repository = format!("{}/{}", repo, img);

    DockerImage {
        registry,
        repository,
        tag,
        img,
        repo,
    }
}

async fn get_auth_token(
    client: &Client,
    auth_url: &str,
    service: &str,
    repository: &str,
    content_type: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let url = format!(
        "{}?service={}&scope=repository:{}:pull",
        auth_url, service, repository
    );

    let resp = client.get(&url).send().await?;
    let auth_data: Value = resp.json().await?;

    Ok(auth_data["token"].as_str().unwrap_or("").to_string())
}

fn progress_bar(digest: &str, progress: usize) {
    let short_digest = &digest[7..19.min(digest.len())];
    print!("\r{}: Downloading [", short_digest);

    for i in 0..progress {
        if i == progress - 1 {
            print!(">");
        } else {
            print!("=");
        }
    }

    for _ in 0..(49 - progress) {
        print!(" ");
    }
    print!("]");
    io::stdout().flush().unwrap();
}

async fn download_layer(
    client: &Client,
    registry: &str,
    repository: &str,
    layer: &Layer,
    token: &str,
    layer_dir: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "https://{}/v2/{}/blobs/{}",
        registry, repository, layer.digest
    );

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {}", token).parse()?,
    );
    headers.insert(
        reqwest::header::ACCEPT,
        "application/vnd.docker.distribution.manifest.v2+json,\
     application/vnd.docker.distribution.manifest.list.v2+json,\
     application/vnd.oci.image.index.v1+json,\
     application/vnd.oci.image.manifest.v1+json"
            .parse()?,
    );

    print!(
        "{}: Downloading...",
        &layer.digest[7..19.min(layer.digest.len())]
    );
    io::stdout().flush().unwrap();

    let mut response = client
        .get(&url)
        .headers(headers.clone())
        .timeout(Duration::from_secs(300))
        .send()
        .await?;

    // Try alternative URL if main URL fails
    if response.status() != 200 {
        if let Some(urls) = &layer.urls {
            if let Some(alt_url) = urls.first() {
                response = client.get(alt_url).headers(headers).send().await?;
            }
        }
    }

    if response.status() != 200 {
        return Err(format!("Failed to download layer: HTTP {}", response.status()).into());
    }

    let content_length = response.content_length().unwrap_or(0);
    let unit = content_length / 50;
    let mut acc = 0u64;
    let mut progress = 0;

    let layer_path = format!("{}/layer_gzip.tar", layer_dir);
    let mut file = AsyncFile::create(&layer_path).await?;

    progress_bar(&layer.digest, progress);

    let mut stream = response.bytes_stream();
    use futures_util::StreamExt;

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result?;
        file.write_all(&chunk).await?;
        acc += chunk.len() as u64;

        if acc > unit && unit > 0 {
            progress = std::cmp::min(progress + 1, 49);
            progress_bar(&layer.digest, progress);
            acc = 0;
        }
    }

    file.flush().await?;
    drop(file);

    print!(
        "\r{}: Extracting...{}",
        &layer.digest[7..19.min(layer.digest.len())],
        " ".repeat(50)
    );
    io::stdout().flush().unwrap();

    // Decompress the gzipped layer
    let compressed_file = File::open(&layer_path)?;
    let decoder = GzDecoder::new(BufReader::new(compressed_file));
    let mut decompressed_file = File::create(format!("{}/layer.tar", layer_dir))?;

    io::copy(
        &mut BufReader::new(decoder),
        &mut BufWriter::new(&mut decompressed_file),
    )?;

    // Remove the compressed file
    fs::remove_file(&layer_path)?;

    println!(
        "\r{}: Pull complete [{}]",
        &layer.digest[7..19.min(layer.digest.len())],
        content_length
    );

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Check command line arguments
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!(
            "Usage:\n\t{} [registry/][repository/]image[:tag|@digest]\n",
            args[0]
        );
        process::exit(1);
    }

    let client = create_client().await?;
    let docker_image = parse_image_name(&args[1]);

    println!("[+] Connecting to registry: {}", docker_image.registry);

    // Check registry and get authentication endpoint
    let registry_url = format!("https://{}/v2/", docker_image.registry);
    let resp = client.get(&registry_url).send().await;

    let (auth_url, reg_service) = match resp {
        Ok(response) if response.status() == 401 => {
            let www_auth = response
                .headers()
                .get("WWW-Authenticate")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("");

            let parts: Vec<&str> = www_auth.split('"').collect();
            let auth_url = if parts.len() > 1 {
                parts[1]
            } else {
                "https://auth.docker.io/token"
            };
            let service = if parts.len() > 3 {
                parts[3]
            } else {
                "registry.docker.io"
            };

            (auth_url.to_string(), service.to_string())
        }
        Ok(_) => (
            "https://auth.docker.io/token".to_string(),
            "registry.docker.io".to_string(),
        ),
        Err(e) => {
            eprintln!("[-] Connection error: {}", e);
            eprintln!("[*] Troubleshooting tips:");
            eprintln!("    1. Check your internet connection");
            eprintln!(
                "    2. If you are behind a proxy, set HTTP_PROXY and HTTPS_PROXY environment variables"
            );
            eprintln!("    3. Try using a VPN if the registry is blocked");
            eprintln!(
                "    4. Verify if the registry {} is accessible from your network",
                docker_image.registry
            );
            process::exit(1);
        }
    };

    // Get authentication token
    println!(
        "[+] Trying to fetch manifest for {}",
        docker_image.repository
    );
    let token = get_auth_token(
        &client,
        &auth_url,
        &reg_service,
        &docker_image.repository,
        "application/vnd.docker.distribution.manifest.v2+json,\
        application/vnd.docker.distribution.manifest.list.v2+json,\
        application/vnd.oci.image.index.v1+json,\
        application/vnd.oci.image.manifest.v1+json",
    ).await?;

    // Fetch manifest
    let manifest_url = format!(
        "https://{}/v2/{}/manifests/{}",
        docker_image.registry, docker_image.repository, docker_image.tag
    );

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {}", token).parse()?,
    );

    headers.insert(
        reqwest::header::ACCEPT,
        "application/vnd.docker.distribution.manifest.v2+json,\
        application/vnd.docker.distribution.manifest.list.v2+json,\
        application/vnd.oci.image.index.v1+json,\
        application/vnd.oci.image.manifest.v1+json"
            .parse()?,
    );

    let resp = client
        .get(&manifest_url)
        .headers(headers.clone())
        .send()
        .await?;

    println!("[+] Response status code: {}", resp.status());

    if resp.status() != 200 {
        eprintln!(
            "[-] Cannot fetch manifest for {} [HTTP {}], URL: {}, HEADER: {:?}",
            docker_image.repository,
            resp.status(),
            &manifest_url,
            headers.clone()
        );
        process::exit(1);
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    println!("[+] Content type: {}", content_type);

    let mut manifest: Value = resp.json().await?;

    // Handle multi-arch manifests
    if let Some(manifests) = manifest.get("manifests") {
        println!("[+] This is a multi-arch image. Available platforms:");

        if let Some(manifests_array) = manifests.as_array() {
            for m in manifests_array {
                if let Some(platform) = m.get("platform") {
                    let os = platform
                        .get("os")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let arch = platform
                        .get("architecture")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let digest = m.get("digest").and_then(|v| v.as_str()).unwrap_or("");
                    println!("    - {}/{} ({})", os, arch, digest);
                }
            }

            // Select linux/amd64 or windows/amd64 platform
            let mut selected_manifest = None;

            // Try linux/amd64 first
            for m in manifests_array {
                if let Some(platform) = m.get("platform") {
                    let os = platform.get("os").and_then(|v| v.as_str()).unwrap_or("");
                    let arch = platform
                        .get("architecture")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if os == "linux" && arch == "amd64" {
                        selected_manifest = Some(m);
                        break;
                    }
                }
            }

            // Fall back to windows/amd64
            if selected_manifest.is_none() {
                for m in manifests_array {
                    if let Some(platform) = m.get("platform") {
                        let os = platform.get("os").and_then(|v| v.as_str()).unwrap_or("");
                        let arch = platform
                            .get("architecture")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if os == "windows" && arch == "amd64" {
                            selected_manifest = Some(m);
                            break;
                        }
                    }
                }
            }

            // Use first if no preferred platform found
            if selected_manifest.is_none() {
                selected_manifest = manifests_array.first();
            }

            if let Some(selected) = selected_manifest {
                if let Some(platform) = selected.get("platform") {
                    let os = platform
                        .get("os")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let arch = platform
                        .get("architecture")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    println!("[+] Selected platform: {}/{}", os, arch);
                }

                // Fetch specific manifest
                if let Some(digest) = selected.get("digest").and_then(|v| v.as_str()) {
                    let specific_manifest_url = format!(
                        "https://{}/v2/{}/manifests/{}",
                        docker_image.registry, docker_image.repository, digest
                    );
                    println!(
                        "[+] Trying to fetch specific manifest: {}",
                        &specific_manifest_url
                    );

                    let token = get_auth_token(
                        &client,
                        &auth_url,
                        &reg_service,
                        &docker_image.repository,
                        "application/vnd.docker.distribution.manifest.v2+json,\
                                    application/vnd.docker.distribution.manifest.list.v2+json,\
                                    application/vnd.oci.image.index.v1+json,\
                                    application/vnd.oci.image.manifest.v1+json",
                    )
                    .await?;

                    let mut headers = reqwest::header::HeaderMap::new();
                    headers.insert(
                        reqwest::header::AUTHORIZATION,
                        format!("Bearer {}", token).parse()?,
                    );
                    headers.insert(
                        reqwest::header::ACCEPT,
                        "application/vnd.docker.distribution.manifest.v2+json,\
                            application/vnd.docker.distribution.manifest.list.v2+json,\
                            application/vnd.oci.image.index.v1+json,\
                            application/vnd.oci.image.manifest.v1+json"
                            .parse()?,
                    );

                    let manifest_resp = client
                        .get(&specific_manifest_url)
                        .headers(headers)
                        .send()
                        .await?;
                    if manifest_resp.status() != 200 {
                        eprintln!(
                            "[-] Failed to fetch specific manifest: {}",
                            manifest_resp.status()
                        );
                        process::exit(1);
                    }
                    manifest = manifest_resp.json().await?;
                    println!("[+] Successfully fetched specific manifest");
                }
            }
        }
    }

    // Extract layers
    let layers_value = manifest
        .get("layers")
        .ok_or("No layers found in manifest")?;
    let layers_array = layers_value.as_array().ok_or("Layers is not an array")?;

    let mut layers = Vec::new();
    for layer_value in layers_array {
        let digest = layer_value
            .get("digest")
            .and_then(|v| v.as_str())
            .ok_or("Layer missing digest")?
            .to_string();

        let urls = layer_value
            .get("urls")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|url| url.as_str().map(|s| s.to_string()))
                    .collect()
            });

        layers.push(Layer { digest, urls });
    }

    // Create tmp directory
    let img_dir = "tmp";
    if !Path::new(img_dir).exists() {
        println!("[+] Creating temporary directory: {}", img_dir);
        fs::create_dir_all(img_dir)?;
    }

    // Download config
    let config_digest = manifest
        .get("config")
        .and_then(|c| c.get("digest"))
        .and_then(|d| d.as_str())
        .ok_or("No config digest found")?;

    let config_url = format!(
        "https://{}/v2/{}/blobs/{}",
        docker_image.registry, docker_image.repository, config_digest
    );
    let config_resp = client
        .get(&config_url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await?;

    let config_path = format!("{}/{}.json", img_dir, &config_digest[7..]);
    let mut config_file = File::create(&config_path)?;
    config_file.write_all(&config_resp.bytes().await?)?;

    // Process layers
    let mut content = json!([{
        "Config": format!("{}.json", &config_digest[7..]),
        "RepoTags": [],
        "Layers": []
    }]);

    // Set RepoTags
    let repo_tag = if docker_image.repo != "library" {
        format!(
            "{}:{}",
            if docker_image.repo.is_empty() {
                docker_image.img.clone()
            } else {
                format!("{}/{}", docker_image.repo, docker_image.img)
            },
            docker_image.tag
        )
    } else {
        format!("{}:{}", docker_image.img, docker_image.tag)
    };
    content[0]["RepoTags"]
        .as_array_mut()
        .unwrap()
        .push(json!(repo_tag));

    let empty_json = json!({
        "created": "1970-01-01T00:00:00Z",
        "container_config": {
            "Hostname": "",
            "Domainname": "",
            "User": "",
            "AttachStdin": false,
            "AttachStdout": false,
            "AttachStderr": false,
            "Tty": false,
            "OpenStdin": false,
            "StdinOnce": false,
            "Env": null,
            "Cmd": null,
            "Image": "",
            "Volumes": null,
            "WorkingDir": "",
            "Entrypoint": null,
            "OnBuild": null,
            "Labels": null
        }
    });

    let mut parent_id = String::new();
    let config_content = fs::read_to_string(&config_path)?;
    let config_json: Value = serde_json::from_str(&config_content)?;

    for (i, layer) in layers.iter().enumerate() {
        // Generate fake layer ID
        let layer_input = format!("{}\n{}\n", parent_id, layer.digest);
        let mut hasher = Sha256::new();
        hasher.update(layer_input.as_bytes());
        let fake_layer_id = format!("{:x}", hasher.finalize());

        let layer_dir = format!("{}/{}", img_dir, fake_layer_id);
        fs::create_dir_all(&layer_dir)?;

        // Create VERSION file
        fs::write(format!("{}/VERSION", layer_dir), "1.0")?;

        // Download layer
        let token = get_auth_token(
            &client,
            &auth_url,
            &reg_service,
            &docker_image.repository,
            "application/vnd.docker.distribution.manifest.v2+json,\
     application/vnd.docker.distribution.manifest.list.v2+json,\
     application/vnd.oci.image.index.v1+json,\
     application/vnd.oci.image.manifest.v1+json",
        )
        .await?;

        download_layer(
            &client,
            &docker_image.registry,
            &docker_image.repository,
            layer,
            &token,
            &layer_dir,
        )
        .await?;

        content[0]["Layers"]
            .as_array_mut()
            .unwrap()
            .push(json!(format!("{}/layer.tar", fake_layer_id)));

        // Create JSON file
        let mut json_obj = if i == layers.len() - 1 {
            // Last layer uses config
            let mut obj = config_json.clone();
            if let Some(obj_map) = obj.as_object_mut() {
                obj_map.remove("history");
                obj_map.remove("rootfs");
                obj_map.remove("rootfS"); // Microsoft case insensitivity
            }
            obj
        } else {
            empty_json.clone()
        };

        if let Some(obj_map) = json_obj.as_object_mut() {
            obj_map.insert("id".to_string(), json!(fake_layer_id.clone()));
            if !parent_id.is_empty() {
                obj_map.insert("parent".to_string(), json!(parent_id.clone()));
            }
        }

        parent_id = fake_layer_id.clone();
        fs::write(
            format!("{}/json", layer_dir),
            serde_json::to_string(&json_obj)?,
        )?;
    }

    // Write manifest.json
    fs::write(
        format!("{}/manifest.json", img_dir),
        serde_json::to_string(&content)?,
    )?;

    // Write repositories file
    let repositories = if &docker_image.repo != "library" && !&docker_image.repo.is_empty() {
        json!({
            format!("{}/{}", &docker_image.repo, &docker_image.img): {
                &docker_image.tag: parent_id
            }
        })
    } else {
        json!({
            &docker_image.img: {
                &docker_image.tag: parent_id
            }
        })
    };
    fs::write(
        format!("{}/repositories", img_dir),
        serde_json::to_string(&repositories)?,
    )?;

    // Create tar archive
    let docker_tar = format!(
        "{}_{}.tar",
        &docker_image.repo.replace('/', "_"),
        docker_image.img
    );
    print!("Creating archive...");
    io::stdout().flush().unwrap();

    let tar_file = File::create(&docker_tar)?;
    let mut tar_builder = tar::Builder::new(tar_file);
    tar_builder.append_dir_all("", img_dir)?;
    tar_builder.finish()?;

    // Clean up tmp directory
    fs::remove_dir_all(img_dir)?;

    println!("\rDocker image pulled: {}", docker_tar);

    Ok(())
}

