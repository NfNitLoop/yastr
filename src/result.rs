use std::error::Error as StdError;
use std::result::Result as StdResult;

pub type Result<T = (), E = Box<dyn StdError>> = StdResult<T, E>;