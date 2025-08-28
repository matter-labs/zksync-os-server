use zksync_os_object_store::ObjectStoreMode::S3WithCredentialFile;
use zksync_os_object_store::{Bucket, ObjectStoreConfig, ObjectStoreFactory, ObjectStoreMode};

#[tokio::main]
async fn main() {
    println!("Hello, world!");

    let config = ObjectStoreConfig {
        mode: ObjectStoreMode::S3WithCredentialFile {
            bucket_base_url: "zksync-os-stage-stage-fri-proofs".to_string(),
            endpoint: Some("https://hel1.your-objectstorage.com".to_string()),
            s3_credential_file_path: "/Users/romanbrodetski/s3.stage.bucket".into(),
            region: None,
        },
        max_retries: 5,
        local_mirror_path: None,
    };

    let object_store = ObjectStoreFactory::new(config)
        .create_store()
        .await
        .unwrap();


    let r = object_store
        .put_raw(Bucket("fri_batch_envelopes"), "temp-test", vec![])
        .await
        .unwrap();

    println!("{:?}", r);
}
