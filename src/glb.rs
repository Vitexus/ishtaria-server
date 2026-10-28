//! A small reader of glTF binaries (`.glb`) that returns the triangles of every mesh, in model
//! space, for collision. Only what a static building needs is understood: node transforms, float
//! positions and indexed or plain triangle lists. Compressed or sparse data is refused.
//! Everything is bounded, since the file comes from a datadisk.

use anyhow::{anyhow, bail, ensure, Result};
use serde_json::Value;

const MAX_TRIANGLES: usize = 400_000;
const MAX_NODES: usize = 4096;
const MAX_DEPTH: usize = 24;

/// Triangles of a model: shared vertices and indices into them.
pub struct Triangles {
    pub vertices: Vec<[f64; 3]>,
    pub triangles: Vec<[u32; 3]>,
}

type Matrix = [[f64; 4]; 4];

const IDENTITY: Matrix = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// `a * b`: apply `b` first, then `a`.
fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    let mut out = [[0.0; 4]; 4];
    for (row, out_row) in out.iter_mut().enumerate() {
        for (column, cell) in out_row.iter_mut().enumerate() {
            *cell = (0..4).map(|k| a[row][k] * b[k][column]).sum();
        }
    }
    out
}

fn transform(matrix: &Matrix, point: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|row| {
        matrix[row][0] * point[0]
            + matrix[row][1] * point[1]
            + matrix[row][2] * point[2]
            + matrix[row][3]
    })
}

fn numbers<const N: usize>(value: Option<&Value>) -> Option<[f64; N]> {
    let array = value?.as_array()?;
    if array.len() != N {
        return None;
    }
    let mut out = [0.0; N];
    for (slot, item) in out.iter_mut().zip(array) {
        *slot = item.as_f64().filter(|number| number.is_finite())?;
    }
    Some(out)
}

/// The local matrix of a node: `matrix`, or translation, rotation (quaternion) and scale.
fn local_matrix(node: &Value) -> Result<Matrix> {
    if let Some(flat) = numbers::<16>(node.get("matrix")) {
        // glTF stores matrices column by column.
        return Ok(std::array::from_fn(|row| {
            std::array::from_fn(|column| flat[column * 4 + row])
        }));
    }
    let translation = numbers::<3>(node.get("translation")).unwrap_or([0.0; 3]);
    let [x, y, z, w] = numbers::<4>(node.get("rotation")).unwrap_or([0.0, 0.0, 0.0, 1.0]);
    let scale = numbers::<3>(node.get("scale")).unwrap_or([1.0; 3]);
    let length = (x * x + y * y + z * z + w * w).sqrt();
    ensure!(length > 1e-9, "a node has a zero rotation");
    let (x, y, z, w) = (x / length, y / length, z / length, w / length);
    let rotation = [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - z * w),
            2.0 * (x * z + y * w),
        ],
        [
            2.0 * (x * y + z * w),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - x * w),
        ],
        [
            2.0 * (x * z - y * w),
            2.0 * (y * z + x * w),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ];
    let mut matrix = IDENTITY;
    for row in 0..3 {
        for column in 0..3 {
            matrix[row][column] = rotation[row][column] * scale[column];
        }
        matrix[row][3] = translation[row];
    }
    Ok(matrix)
}

struct Document<'a> {
    json: Value,
    binary: &'a [u8],
}

impl Document<'_> {
    fn array(&self, name: &str, index: usize) -> Result<&Value> {
        self.json
            .get(name)
            .and_then(Value::as_array)
            .and_then(|items| items.get(index))
            .ok_or_else(|| anyhow!("missing {name}[{index}]"))
    }

    /// The bytes of an accessor with the stride of its elements.
    fn accessor(
        &self,
        index: usize,
        components: usize,
        allowed: &[u64],
    ) -> Result<(Vec<f64>, usize)> {
        let accessor = self.array("accessors", index)?;
        ensure!(
            accessor.get("sparse").is_none(),
            "sparse accessors are not supported"
        );
        let count = accessor
            .get("count")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("accessor without count"))? as usize;
        ensure!(count <= MAX_TRIANGLES * 3, "accessor is too large");
        let component_type = accessor
            .get("componentType")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        ensure!(
            allowed.contains(&component_type),
            "unsupported component type {component_type}"
        );
        let size = match component_type {
            5121 => 1,
            5123 => 2,
            5125 | 5126 => 4,
            _ => bail!("unsupported component type {component_type}"),
        };
        let view = self.array(
            "bufferViews",
            accessor
                .get("bufferView")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("accessor without a buffer view"))? as usize,
        )?;
        ensure!(
            view.get("buffer").and_then(Value::as_u64) == Some(0),
            "only the embedded buffer is supported"
        );
        let view_offset = view.get("byteOffset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let accessor_offset = accessor
            .get("byteOffset")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let stride = view
            .get("byteStride")
            .and_then(Value::as_u64)
            .map_or(components * size, |stride| stride as usize);
        ensure!(stride >= components * size, "stride is too small");
        let start = view_offset + accessor_offset;
        let mut values = Vec::with_capacity(count * components);
        for element in 0..count {
            for component in 0..components {
                let at = start + element * stride + component * size;
                let bytes = self
                    .binary
                    .get(at..at + size)
                    .ok_or_else(|| anyhow!("accessor reads past the buffer"))?;
                values.push(match component_type {
                    5121 => f64::from(bytes[0]),
                    5123 => f64::from(u16::from_le_bytes([bytes[0], bytes[1]])),
                    5125 => f64::from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
                    _ => f64::from(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
                });
            }
        }
        Ok((values, count))
    }
}

/// The triangles of all meshes of the default scene, transformed by their nodes.
pub fn triangles(bytes: &[u8]) -> Result<Triangles> {
    ensure!(
        bytes.len() >= 20 && &bytes[0..4] == b"glTF",
        "not a glTF binary"
    );
    ensure!(
        u32::from_le_bytes(bytes[4..8].try_into()?) == 2,
        "only glTF 2 is supported"
    );
    let (mut json, mut binary): (Option<&[u8]>, Option<&[u8]>) = (None, None);
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let length = u32::from_le_bytes(bytes[at..at + 4].try_into()?) as usize;
        let kind = &bytes[at + 4..at + 8];
        let body = bytes
            .get(at + 8..at + 8 + length)
            .ok_or_else(|| anyhow!("a chunk is cut short"))?;
        match kind {
            b"JSON" if json.is_none() => json = Some(body),
            b"BIN\0" if binary.is_none() => binary = Some(body),
            _ => {}
        }
        at += 8 + length.next_multiple_of(4);
    }
    let document = Document {
        json: serde_json::from_slice(json.ok_or_else(|| anyhow!("no JSON chunk"))?)?,
        binary: binary.unwrap_or(&[]),
    };
    if let Some(extension) = document
        .json
        .get("extensionsRequired")
        .and_then(Value::as_array)
        .and_then(|required| required.first())
    {
        bail!("required extension {extension} is not supported");
    }
    let scene = document
        .json
        .get("scene")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let roots: Vec<usize> = document
        .array("scenes", scene)?
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| node.as_u64().map(|node| node as usize))
        .collect();
    let mut out = Triangles {
        vertices: Vec::new(),
        triangles: Vec::new(),
    };
    let mut visited = 0;
    let mut stack: Vec<(usize, Matrix, usize)> =
        roots.into_iter().map(|node| (node, IDENTITY, 0)).collect();
    while let Some((index, parent, depth)) = stack.pop() {
        visited += 1;
        ensure!(
            visited <= MAX_NODES && depth <= MAX_DEPTH,
            "the scene is too large or too deep"
        );
        let node = document.array("nodes", index)?;
        let matrix = multiply(&parent, &local_matrix(node)?);
        if let Some(mesh) = node.get("mesh").and_then(Value::as_u64) {
            for primitive in document
                .array("meshes", mesh as usize)?
                .get("primitives")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if primitive.get("mode").and_then(Value::as_u64).unwrap_or(4) != 4 {
                    continue;
                }
                let position = primitive
                    .get("attributes")
                    .and_then(|attributes| attributes.get("POSITION"))
                    .and_then(Value::as_u64)
                    .ok_or_else(|| anyhow!("a primitive without positions"))?
                    as usize;
                let (coordinates, count) = document.accessor(position, 3, &[5126])?;
                let base = out.vertices.len() as u32;
                for point in coordinates.chunks_exact(3) {
                    out.vertices
                        .push(transform(&matrix, [point[0], point[1], point[2]]));
                }
                let indices: Vec<u32> = match primitive.get("indices").and_then(Value::as_u64) {
                    Some(accessor) => {
                        let (values, _) =
                            document.accessor(accessor as usize, 1, &[5121, 5123, 5125])?;
                        values.into_iter().map(|value| value as u32).collect()
                    }
                    None => (0..count as u32).collect(),
                };
                ensure!(
                    indices.len() % 3 == 0,
                    "a primitive has a broken index list"
                );
                ensure!(
                    indices.iter().all(|index| (*index as usize) < count),
                    "an index is out of range"
                );
                // A mirrored node turns the triangles inside out; keep the winding right.
                let determinant = matrix[0][0]
                    * (matrix[1][1] * matrix[2][2] - matrix[1][2] * matrix[2][1])
                    - matrix[0][1] * (matrix[1][0] * matrix[2][2] - matrix[1][2] * matrix[2][0])
                    + matrix[0][2] * (matrix[1][0] * matrix[2][1] - matrix[1][1] * matrix[2][0]);
                for triangle in indices.chunks_exact(3) {
                    out.triangles.push(if determinant < 0.0 {
                        [base + triangle[0], base + triangle[2], base + triangle[1]]
                    } else {
                        [base + triangle[0], base + triangle[1], base + triangle[2]]
                    });
                }
                ensure!(
                    out.triangles.len() <= MAX_TRIANGLES,
                    "the model has too many triangles"
                );
            }
        }
        for child in node
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(child) = child.as_u64() {
                stack.push((child as usize, matrix, depth + 1));
            }
        }
    }
    ensure!(!out.triangles.is_empty(), "the model has no triangles");
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A glb with one triangle on a node translated by (0, 2, 0) and scaled by 2.
    pub(crate) fn sample(index_type: u32) -> Vec<u8> {
        let mut binary = Vec::new();
        for value in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0] {
            binary.extend(value.to_le_bytes());
        }
        let indices_at = binary.len();
        let indices: Vec<u8> = if index_type == 5123 {
            [0u16, 1, 2]
                .iter()
                .flat_map(|index| index.to_le_bytes())
                .collect()
        } else {
            [0u32, 1, 2]
                .iter()
                .flat_map(|index| index.to_le_bytes())
                .collect()
        };
        binary.extend(&indices);
        while binary.len() % 4 != 0 {
            binary.push(0);
        }
        let json = format!(
            r#"{{"asset":{{"version":"2.0"}},"scene":0,"scenes":[{{"nodes":[0]}}],
            "nodes":[{{"mesh":0,"translation":[0,2,0],"scale":[2,2,2]}}],
            "meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1}}]}}],
            "accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3"}},
            {{"bufferView":1,"componentType":{index_type},"count":3,"type":"SCALAR"}}],
            "bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36}},{{"buffer":0,"byteOffset":{indices_at},"byteLength":{}}}],
            "buffers":[{{"byteLength":{}}}]}}"#,
            indices.len(),
            binary.len()
        );
        let mut json = json.into_bytes();
        while json.len() % 4 != 0 {
            json.push(b' ');
        }
        let total = 12 + 8 + json.len() + 8 + binary.len();
        let mut out = Vec::new();
        out.extend(b"glTF");
        out.extend(2u32.to_le_bytes());
        out.extend((total as u32).to_le_bytes());
        out.extend((json.len() as u32).to_le_bytes());
        out.extend(b"JSON");
        out.extend(&json);
        out.extend((binary.len() as u32).to_le_bytes());
        out.extend(b"BIN\0");
        out.extend(&binary);
        out
    }

    #[test]
    fn reads_an_indexed_triangle_with_its_node_transform() {
        for index_type in [5123, 5125] {
            let result = triangles(&sample(index_type)).unwrap();
            assert_eq!(result.triangles, vec![[0, 1, 2]]);
            assert_eq!(result.vertices[1], [2.0, 2.0, 0.0]);
            assert_eq!(result.vertices[2], [0.0, 2.0, 2.0]);
        }
    }

    #[test]
    fn refuses_garbage_and_truncated_files() {
        assert!(triangles(b"not a model at all").is_err());
        let good = sample(5123);
        assert!(triangles(&good[..good.len() - 10]).is_err());
        let mut wrong_version = good.clone();
        wrong_version[4] = 1;
        assert!(triangles(&wrong_version).is_err());
    }

    /// `GLB=path cargo test glb_stats -- --ignored --nocapture` prints the size of a real model.
    #[test]
    #[ignore = "needs a model file in GLB"]
    fn glb_stats() {
        let Ok(path) = std::env::var("GLB") else {
            return; // a utility: it needs GLB=path
        };
        let started = std::time::Instant::now();
        let result = triangles(&std::fs::read(path).unwrap()).unwrap();
        let mut low = [f64::MAX; 3];
        let mut high = [f64::MIN; 3];
        for point in &result.vertices {
            for axis in 0..3 {
                low[axis] = low[axis].min(point[axis]);
                high[axis] = high[axis].max(point[axis]);
            }
        }
        println!(
            "{} vertices, {} triangles, min {low:?}, max {high:?}, {:?}",
            result.vertices.len(),
            result.triangles.len(),
            started.elapsed()
        );
    }
}
