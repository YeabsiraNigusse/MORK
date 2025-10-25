
use std::time::Duration;
use std::sync::Mutex;

use hyper::StatusCode;

use pathmap::{PathMap, zipper::ZipperHeadOwned};
use pathmap::zipper_tracking::Conflict;
use pathmap::zipper::*;

use mork::{PermissionArb, PathPermissionErr, Space, SpaceReaderZipper, SpaceWriterZipper};

use crate::status_map::*;
use crate::commands::*;

/// The time to wait before rejecting a request with a conflicted path
const SETTLE_TIME: Duration = Duration::from_millis(5);

/// Weight combination strategies for transformations
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WeightCombination {
    /// Add weights together
    Add,
    /// Multiply weights
    Multiply,
    /// Take the maximum weight
    Max,
    /// Take the minimum weight
    Min,
    /// Take the average weight
    Average,
}

impl Default for WeightCombination {
    fn default() -> Self {
        WeightCombination::Add
    }
}

/// A space that stores f64 weights instead of unit values
pub struct WeightedSpace {  
    /// The global symbol table used by the primary map
    global_symbol_table: bucket_map::SharedMappingHandle,  
    /// ZipperHead for accessing the primary map with f64 weights
    primary_map: ZipperHeadOwned<f64>,  
    /// ZipperHead for accessing status and permissions associated with paths
    pub(crate) status_map: StatusMap,  
    /// Guard to ensure high-level operations can be atomic
    permission_guard: Mutex<()>,  
    /// Strategy for combining weights during transformations
    weight_combination: WeightCombination,
}

/// Read Permission object in a [WeightedSpace]
pub struct WeightedSpaceReader<'space>(ReadZipperTracked<'space, 'static, f64>);

/// Write Permission object in a [WeightedSpace]
pub struct WeightedSpaceWriter<'space> {
    z: WriteZipperTracked<'space, 'static, f64>,
    zh: ZipperHeadOwned<f64>,
}

/// PermissionHead object for [WeightedSpace]
pub struct WeightedPermissionHead<'space>(&'space WeightedSpace);

impl WeightedSpace {
    /// Make a new `WeightedSpace`, loading it from the snapshot
    pub fn new() -> Self {
        Self::with_weight_combination(WeightCombination::default())
    }

    /// Make a new `WeightedSpace` with a specific weight combination strategy
    pub fn with_weight_combination(weight_combination: WeightCombination) -> Self {
        // Load the PathMap from the last snapshot
        //GOAT, Actually load it!!
        let primary_map = PathMap::<f64>::new();
        let primary_map = primary_map.into_zipper_head([]);

        // Load the status map also
        //GOAT, Load this from the snapshot
        let status_map = StatusMap::new();

        // init symbol table
        //GOAT, Load this from the snapshot
        let global_symbol_table = bucket_map::SharedMapping::new();

        Self {
            global_symbol_table,
            primary_map,
            status_map,
            permission_guard: Mutex::new(()),
            weight_combination,
        }
    }

    /// Get the current weight combination strategy
    pub fn weight_combination(&self) -> WeightCombination {
        self.weight_combination
    }

    /// Set the weight combination strategy
    pub fn set_weight_combination(&mut self, strategy: WeightCombination) {
        self.weight_combination = strategy;
    }

    /// Combine weights according to the current strategy
    pub fn combine_weights(&self, weights: &[f64]) -> f64 {
        if weights.is_empty() {
            return 0.0;
        }

        match self.weight_combination {
            WeightCombination::Add => weights.iter().sum(),
            WeightCombination::Multiply => weights.iter().product(),
            WeightCombination::Max => weights.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b)),
            WeightCombination::Min => weights.iter().fold(f64::INFINITY, |a, &b| a.min(b)),
            WeightCombination::Average => weights.iter().sum::<f64>() / weights.len() as f64,
        }
    }

    /// Set a weight at a specific path
    pub fn set_weight(&mut self, path: &[u8], weight: f64) -> Result<(), WeightedPermissionErr> {
        let mut writer = self.new_writer(path, &())?;
        let mut wz = self.write_zipper(&mut writer);
        wz.set_val(weight);
        Ok(())
    }

    /// Get a weight at a specific path
    pub fn get_weight(&self, path: &[u8]) -> Result<Option<f64>, WeightedPermissionErr> {
        let mut reader = self.new_reader(path, &())?;
        let mut rz = self.read_zipper(&mut reader);
        if rz.to_next_val() {
            Ok(Some(rz.val()))
        } else {
            Ok(None)
        }
    }

    /// Add to an existing weight at a path, or set it if it doesn't exist
    pub fn add_weight(&mut self, path: &[u8], weight: f64) -> Result<f64, WeightedPermissionErr> {
        let mut writer = self.new_writer(path, &())?;
        let mut wz = self.write_zipper(&mut writer);
        let current_weight = wz.val().unwrap_or(0.0);
        let new_weight = current_weight + weight;
        wz.set_val(new_weight);
        Ok(new_weight)
    }

    /// Multiply an existing weight at a path, or set it if it doesn't exist
    pub fn multiply_weight(&mut self, path: &[u8], factor: f64) -> Result<f64, WeightedPermissionErr> {
        let mut writer = self.new_writer(path, &())?;
        let mut wz = self.write_zipper(&mut writer);
        let current_weight = wz.val().unwrap_or(1.0);
        let new_weight = current_weight * factor;
        wz.set_val(new_weight);
        Ok(new_weight)
    }

    /// Get the total weight of all values in the space
    pub fn total_weight(&self) -> Result<f64, WeightedPermissionErr> {
        let mut reader = self.new_reader(&[], &())?;
        let mut rz = self.read_zipper(&mut reader);
        let mut total = 0.0;
        while rz.to_next_val() {
            total += rz.val();
        }
        Ok(total)
    }

    /// Get the count of weighted entries in the space
    pub fn weight_count(&self) -> Result<usize, WeightedPermissionErr> {
        let mut reader = self.new_reader(&[], &())?;
        let mut rz = self.read_zipper(&mut reader);
        let mut count = 0;
        while rz.to_next_val() {
            count += 1;
        }
        Ok(count)
    }
    pub fn get_status<P: AsRef<[u8]>>(&self, path: P) -> StatusRecord {
        self.status_map.get_status(path.as_ref())
    }
    
    pub fn set_user_status<P: AsRef<[u8]>>(&self, path: P, new_status: StatusRecord) -> Result<(), CommandError> {
        let path = path.as_ref();
        self.status_map.try_set_user_status(path, new_status)
            .map_err(|err_status_rec| CommandError::External(ExternalError::new(StatusCode::UNAUTHORIZED, format!("Conflicting status: {err_status_rec:?} at path: {path:?} when attempting to set new status"))))
    }
    
    /// Wrapper around direct method to acquire WritePermission, waiting SETTLE_TIME for previous requests to
    /// settle before rejecting the request
    pub async fn new_writer_async<'space>(&'space self, path: &[u8], auth: &()) -> Result<WeightedSpaceWriter<'space>, WeightedPermissionErr> {
        match self.new_writer(path, auth) {
            Ok(perm) => Ok(perm),
            Err(_) => {
                tokio::time::sleep(SETTLE_TIME).await;
                self.new_writer(path, auth)
            }
        }
    }
    
    /// See `new_writer_async`
    pub async fn new_reader_async<'space>(&'space self, path: &[u8], auth: &()) -> Result<WeightedSpaceReader<'space>, WeightedPermissionErr> {
        match self.new_reader(path, auth) {
            Ok(perm) => Ok(perm),
            Err(_) => {
                tokio::time::sleep(SETTLE_TIME).await;
                self.new_reader(path, auth)
            }
        }
    }
}

impl<'space> PermissionArb<'space, WeightedSpace> for WeightedPermissionHead<'space> {
    fn new_reader(&self, path: &[u8], _auth: &()) -> Result<WeightedSpaceReader<'space>, WeightedPermissionErr> {
        let reader = WeightedSpaceReader(self.0.primary_map.read_zipper_at_path(path).map_err(|e| {
            WeightedPermissionErr {
                message: format!("Conflict trying to acquire read zipper at {path:?}, {e}"),
                path: path.to_vec()
            }
        })?);
        Ok(reader)
    }

    /// Requests a new [Space::Writer] from the `Space`
    fn new_writer(&self, path: &[u8], _auth: &()) -> Result<WeightedSpaceWriter<'space>, WeightedPermissionErr> {
        let writer = WeightedSpaceWriter {
            z: self.0.primary_map.write_zipper_at_exclusive_path(path).map_err(|e| {
                WeightedPermissionErr {
                    message: format!("Conflict trying to acquire write zipper at {path:?}, {e}"),
                    path: path.to_vec()
                }
            })?,
            zh: self.0.primary_map.clone(),
        };
        Ok(writer)
    }
}

/// [PathPermissionErr] in a [WeightedSpace]
#[derive(Debug)]
pub struct WeightedPermissionErr {
    path: Vec<u8>,
    message: String,
}

impl WeightedPermissionErr {
    pub fn new(path: &[u8], message: String) -> Self {
        Self {path: path.to_vec(), message}
    }
    pub fn from_conflict(conflict: Conflict, path: &[u8]) -> Self {
        let nice_path = mork_bytestring::serialize(path);
        let nice_existing_path = mork_bytestring::serialize(conflict.path());
        Self {
            path: path.to_vec(),
            message: format!("{conflict} trying to take path: `{nice_path}` while `{nice_existing_path}` was already taken")
        }
    }
}

impl From<WeightedPermissionErr> for CommandError {
    fn from(perm_err: WeightedPermissionErr) -> Self {
        CommandError::External(ExternalError::new(StatusCode::UNAUTHORIZED, format!("Permission error accessing path: {perm_err:?}")))
    }
}

impl PathPermissionErr for WeightedPermissionErr {
    fn path(&self) -> &[u8] {
        &self.path
    }
}

impl core::fmt::Display for WeightedPermissionErr {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        self.message.fmt(f)
    }
}

impl Space for WeightedSpace {
    type Auth = ();
    type Reader<'space> = WeightedSpaceReader<'space>;
    type Writer<'space> = WeightedSpaceWriter<'space>;
    type PermissionHead<'space> = WeightedPermissionHead<'space>;
    type PermissionErr = WeightedPermissionErr;

    fn new_multiple<'space, F: FnOnce(&Self::PermissionHead<'space>)->Result<(), Self::PermissionErr>>(&'space self, f: F) -> Result<(), Self::PermissionErr> {
        let guard = self.permission_guard.lock().unwrap();
        let perm_head = WeightedPermissionHead(self);
        f(&perm_head)?;
        drop(guard);
        Ok(())
    }
    
    fn read_zipper<'r, 's: 'r>(&'s self, reader: &'r mut Self::Reader<'s>) -> impl SpaceReaderZipper<'s> {
        unsafe{ self.primary_map.read_zipper_at_borrowed_path_unchecked(reader.0.path()) }
    }
    
    fn write_zipper<'w, 's: 'w>(&'s self, writer: &'w mut Self::Writer<'s>) -> impl SpaceWriterZipper + 'w {
        unsafe{ self.primary_map.write_zipper_at_exclusive_path_unchecked(writer.z.path()) }
    }
    
    fn cleanup_write_zipper(&self, wz: impl SpaceWriterZipper) {
        self.primary_map.cleanup_write_zipper(wz);
    }
    
    fn symbol_table(&self) -> &bucket_map::SharedMappingHandle {
        &self.global_symbol_table
    }
}

impl Drop for WeightedSpaceWriter<'_> {
    fn drop(&mut self) {
        self.zh.cleanup_write_zipper(&mut self.z);
    }
}

// Keep the original ServerSpace implementation for backward compatibility
impl<'space> PermissionArb<'space, ServerSpace> for ServerPermissionHead<'space> {
    fn new_reader(&self, path: &[u8], _auth: &()) -> Result<ReadPermission, ServerPermissionErr> {
        self.0.status_map.get_read_permission(&path)
    }

    /// Requests a new [Space::Writer] from the `Space`
    fn new_writer(&self, path: &[u8], _auth: &()) -> Result<WritePermission, ServerPermissionErr> {
        self.0.status_map.get_write_permission(&path)
    }
}

/// [PathPermissionErr] in a [ServerSpace]
#[derive(Debug)]
pub struct ServerPermissionErr {
    path: Vec<u8>,
    message: String,
}

impl ServerPermissionErr {
    pub fn new(path: &[u8], message: String) -> Self {
        Self {path: path.to_vec(), message}
    }
    pub fn from_conflict(conflict: Conflict, path: &[u8]) -> Self {
        let nice_path = mork_bytestring::serialize(path);
        let nice_existing_path = mork_bytestring::serialize(conflict.path());
        Self {
            path: path.to_vec(),
            message: format!("{conflict} trying to take path: `{nice_path}` while `{nice_existing_path}` was already taken")
        }
    }
}

impl From<ServerPermissionErr> for CommandError {
    fn from(perm_err: ServerPermissionErr) -> Self {
        CommandError::External(ExternalError::new(StatusCode::UNAUTHORIZED, format!("Permission error accessing path: {perm_err:?}")))
    }
}

impl PathPermissionErr for ServerPermissionErr {
    fn path(&self) -> &[u8] {
        &self.path
    }
}

impl core::fmt::Display for ServerPermissionErr {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        self.message.fmt(f)
    }
}

impl Space for ServerSpace {
    type Auth = ();
    type Reader<'space> = ReadPermission;
    type Writer<'space> = WritePermission;
    type PermissionHead<'space> = ServerPermissionHead<'space>;
    type PermissionErr = ServerPermissionErr;

    fn new_multiple<'space, F: FnOnce(&Self::PermissionHead<'space>)->Result<(), Self::PermissionErr>>(&'space self, f: F) -> Result<(), Self::PermissionErr> {
        let guard = self.permission_guard.lock().unwrap();
        let perm_head = ServerPermissionHead(self);
        f(&perm_head)?;
        drop(guard);
        Ok(())
    }
    fn read_zipper<'r, 's: 'r>(&'s self, reader: &'r mut Self::Reader<'s>) -> impl SpaceReaderZipper<'s> {
        unsafe{ self.primary_map.read_zipper_at_borrowed_path_unchecked(reader.path()) }
    }
    
    fn write_zipper<'w, 's: 'w>(&'s self, writer: &'w mut Self::Writer<'s>) -> impl SpaceWriterZipper + 'w {
        unsafe{ self.primary_map.write_zipper_at_exclusive_path_unchecked(writer.path()) }
    }
    
    fn cleanup_write_zipper(&self, wz: impl SpaceWriterZipper) {
        self.primary_map.cleanup_write_zipper(wz);
    }
    
    fn symbol_table(&self) -> &bucket_map::SharedMappingHandle {
        &self.global_symbol_table
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_weighted_space_creation() {
        let space = WeightedSpace::new();
        assert_eq!(space.weight_combination(), WeightCombination::Add);
        
        let space_max = WeightedSpace::with_weight_combination(WeightCombination::Max);
        assert_eq!(space_max.weight_combination(), WeightCombination::Max);
    }

    #[test]
    fn test_weight_combination_strategies() {
        let weights = vec![2.0, 3.0, 4.0];
        
        let mut space = WeightedSpace::with_weight_combination(WeightCombination::Add);
        assert_eq!(space.combine_weights(&weights), 9.0);
        
        space.set_weight_combination(WeightCombination::Multiply);
        assert_eq!(space.combine_weights(&weights), 24.0);
        
        space.set_weight_combination(WeightCombination::Max);
        assert_eq!(space.combine_weights(&weights), 4.0);
        
        space.set_weight_combination(WeightCombination::Min);
        assert_eq!(space.combine_weights(&weights), 2.0);
        
        space.set_weight_combination(WeightCombination::Average);
        assert_eq!(space.combine_weights(&weights), 3.0);
    }

    #[test]
    fn test_empty_weights() {
        let space = WeightedSpace::new();
        assert_eq!(space.combine_weights(&[]), 0.0);
    }

    #[test]
    fn test_set_and_get_weight() {
        let mut space = WeightedSpace::new();
        let path = b"test_path";
        let weight = 42.5;
        
        // Initially no weight
        assert_eq!(space.get_weight(path).unwrap(), None);
        
        // Set weight
        space.set_weight(path, weight).unwrap();
        
        // Get weight
        assert_eq!(space.get_weight(path).unwrap(), Some(weight));
    }

    #[test]
    fn test_add_weight() {
        let mut space = WeightedSpace::new();
        let path = b"test_path";
        
        // Add to non-existent weight (should default to 0.0)
        let result = space.add_weight(path, 10.0).unwrap();
        assert_eq!(result, 10.0);
        assert_eq!(space.get_weight(path).unwrap(), Some(10.0));
        
        // Add to existing weight
        let result = space.add_weight(path, 5.0).unwrap();
        assert_eq!(result, 15.0);
        assert_eq!(space.get_weight(path).unwrap(), Some(15.0));
    }

    #[test]
    fn test_multiply_weight() {
        let mut space = WeightedSpace::new();
        let path = b"test_path";
        
        // Multiply non-existent weight (should default to 1.0)
        let result = space.multiply_weight(path, 3.0).unwrap();
        assert_eq!(result, 3.0);
        assert_eq!(space.get_weight(path).unwrap(), Some(3.0));
        
        // Multiply existing weight
        let result = space.multiply_weight(path, 2.0).unwrap();
        assert_eq!(result, 6.0);
        assert_eq!(space.get_weight(path).unwrap(), Some(6.0));
    }

    #[test]
    fn test_total_weight() {
        let mut space = WeightedSpace::new();
        
        // Empty space
        assert_eq!(space.total_weight().unwrap(), 0.0);
        
        // Add some weights
        space.set_weight(b"path1", 10.0).unwrap();
        space.set_weight(b"path2", 20.0).unwrap();
        space.set_weight(b"path3", 30.0).unwrap();
        
        assert_eq!(space.total_weight().unwrap(), 60.0);
    }

    #[test]
    fn test_weight_count() {
        let mut space = WeightedSpace::new();
        
        // Empty space
        assert_eq!(space.weight_count().unwrap(), 0);
        
        // Add some weights
        space.set_weight(b"path1", 10.0).unwrap();
        space.set_weight(b"path2", 20.0).unwrap();
        space.set_weight(b"path3", 30.0).unwrap();
        
        assert_eq!(space.weight_count().unwrap(), 3);
    }

    #[test]
    fn test_multiple_paths() {
        let mut space = WeightedSpace::new();
        
        let paths = [b"path1", b"path2", b"path3"];
        let weights = [1.5, 2.5, 3.5];
        
        // Set weights for multiple paths
        for (path, weight) in paths.iter().zip(weights.iter()) {
            space.set_weight(path, *weight).unwrap();
        }
        
        // Verify all weights are set correctly
        for (path, expected_weight) in paths.iter().zip(weights.iter()) {
            assert_eq!(space.get_weight(path).unwrap(), Some(*expected_weight));
        }
        
        // Verify total weight
        assert_eq!(space.total_weight().unwrap(), 7.5);
        assert_eq!(space.weight_count().unwrap(), 3);
    }

    #[test]
    fn test_weight_overwrite() {
        let mut space = WeightedSpace::new();
        let path = b"test_path";
        
        // Set initial weight
        space.set_weight(path, 10.0).unwrap();
        assert_eq!(space.get_weight(path).unwrap(), Some(10.0));
        
        // Overwrite with new weight
        space.set_weight(path, 20.0).unwrap();
        assert_eq!(space.get_weight(path).unwrap(), Some(20.0));
    }

    #[test]
    fn test_negative_weights() {
        let mut space = WeightedSpace::new();
        let path = b"test_path";
        
        // Test negative weights
        space.set_weight(path, -5.0).unwrap();
        assert_eq!(space.get_weight(path).unwrap(), Some(-5.0));
        
        // Add to negative weight
        let result = space.add_weight(path, 10.0).unwrap();
        assert_eq!(result, 5.0);
        assert_eq!(space.get_weight(path).unwrap(), Some(5.0));
    }

    #[test]
    fn test_zero_weights() {
        let mut space = WeightedSpace::new();
        let path = b"test_path";
        
        // Test zero weight
        space.set_weight(path, 0.0).unwrap();
        assert_eq!(space.get_weight(path).unwrap(), Some(0.0));
        
        // Add to zero weight
        let result = space.add_weight(path, 5.0).unwrap();
        assert_eq!(result, 5.0);
    }

    #[test]
    fn test_fractional_weights() {
        let mut space = WeightedSpace::new();
        let path = b"test_path";
        
        // Test fractional weights
        space.set_weight(path, 3.14159).unwrap();
        assert_eq!(space.get_weight(path).unwrap(), Some(3.14159));
        
        // Multiply fractional weights
        let result = space.multiply_weight(path, 2.0).unwrap();
        assert!((result - 6.28318).abs() < 1e-5);
    }
}

/// Example usage of WeightedSpace
/// 
/// This example demonstrates how to use WeightedSpace for storing and managing
/// weighted expressions with different combination strategies.
/// 
/// # Example
/// 
/// ```rust
/// use mork_server::weighted_space::{WeightedSpace, WeightCombination};
/// 
/// fn main() -> Result<(), Box<dyn std::error::Error>> {
///     // Create a new WeightedSpace with default (Add) combination strategy
///     let mut space = WeightedSpace::new();
///     
///     // Set weights for different expressions
///     space.set_weight(b"expression1", 10.5)?;
///     space.set_weight(b"expression2", 20.0)?;
///     space.set_weight(b"expression3", 5.5)?;
///     
///     // Get individual weights
///     println!("Weight of expression1: {:?}", space.get_weight(b"expression1")?);
///     
///     // Add to existing weight
///     let new_weight = space.add_weight(b"expression1", 5.0)?;
///     println!("New weight after adding 5.0: {}", new_weight);
///     
///     // Get total weight of all expressions
///     println!("Total weight: {}", space.total_weight()?);
///     
///     // Change combination strategy to Max
///     space.set_weight_combination(WeightCombination::Max);
///     
///     // Test weight combination with different strategies
///     let weights = vec![10.0, 20.0, 5.0];
///     println!("Max combination: {}", space.combine_weights(&weights));
///     
///     space.set_weight_combination(WeightCombination::Multiply);
///     println!("Multiply combination: {}", space.combine_weights(&weights));
///     
///     Ok(())
/// }
/// ```
/// 
/// # Weight Combination Strategies
/// 
/// - **Add**: Sum all weights (default)
/// - **Multiply**: Multiply all weights
/// - **Max**: Take the maximum weight
/// - **Min**: Take the minimum weight  
/// - **Average**: Take the average of all weights
/// 
/// # Key Features
/// 
/// - **Type Safety**: Uses f64 for weights instead of unit values
/// - **Flexible Combination**: Multiple strategies for combining weights during transformations
/// - **Backward Compatibility**: ServerSpace remains unchanged for existing code
/// - **Thread Safety**: Uses mutex guards for atomic operations
/// - **Error Handling**: Comprehensive error types for permission and path conflicts
/// 
/// # Use Cases
/// 
/// - **Machine Learning**: Store confidence scores or probabilities for expressions
/// - **Graph Analytics**: Weight edges or nodes in knowledge graphs
/// - **Recommendation Systems**: Store relevance scores for items
/// - **Fuzzy Logic**: Store membership degrees for fuzzy sets
/// - **Probabilistic Programming**: Store probability distributions






// so the plan is

// to understand the space trait
// how the server space and defualt space implemented the space trait
// differenciate what features we need for weighted-space space and impelement the space trait
// write hard codded test for weighted space