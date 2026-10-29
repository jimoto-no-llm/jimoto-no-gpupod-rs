//! ダミー GPU 在庫。コンテナ・ストレージ・ネットワーク・計測は本物のまま。

use gpupod_common::GPU;

/// Python 版 mock.mock_gpus と同一の 3 枚。
pub fn mock_gpus() -> Vec<GPU> {
    vec![
        GPU {
            id: "mock-nvidia".into(),
            vendor: "nvidia".into(),
            name: "MOCK NVIDIA GPU".into(),
            backend: "nvidia".into(),
            selector: "GPU-MOCK".into(),
            devices: Vec::new(),
            memory_mib: None,
        },
        GPU {
            id: "mock-amd".into(),
            vendor: "amd".into(),
            name: "MOCK AMD GPU".into(),
            backend: "device".into(),
            selector: String::new(),
            devices: vec!["/dev/dri/renderD128".into()],
            memory_mib: None,
        },
        GPU {
            id: "mock-intel".into(),
            vendor: "intel".into(),
            name: "MOCK Intel GPU".into(),
            backend: "device".into(),
            selector: String::new(),
            devices: vec!["/dev/dri/renderD129".into()],
            memory_mib: None,
        },
    ]
}
