//! virtual filesystems and filesystem interface

use crate::{
    fs::tar::TarIterator,
    types::{
        errno::Errno,
        file::Permissions,
        UserID, GroupID,
    },
};
use alloc::{
    boxed::Box,
    string::{String, ToString},
    vec, vec::Vec,
};
use super::tree::{
    File, Directory, SymLink,
    get_directory_from_path, get_file_from_path,
};

pub struct DirEnt<'a> {
    serial: usize,
    name: String,
    directory: &'a mut Box<dyn Directory>,
}

/// root directory of our filesystem
pub static mut ROOT_DIR: Option<Box<dyn Directory>> = None;

pub struct VfsRoot {
    files: Vec<Box<dyn File>>,
    directories: Vec<Box<dyn Directory>>,
    links: Vec<Box<dyn SymLink>>,
}

impl Directory for VfsRoot {
    fn get_permissions(&self) -> Permissions {
        Permissions::OwnerRead | Permissions::OwnerWrite | Permissions::GroupRead | Permissions::GroupWrite | Permissions::OtherRead
    }

    fn get_files(&self) -> &Vec<Box<dyn File>> {
        &self.files
    }

    fn get_files_mut(&mut self) -> &mut Vec<Box<dyn File>> {
        &mut self.files
    }

    fn get_directories(&self) -> &Vec<Box<dyn Directory>> {
        &self.directories
    }

    fn get_directories_mut(&mut self) -> &mut Vec<Box<dyn Directory>> {
        &mut self.directories
    }

    fn get_links(&self) -> &Vec<Box<dyn SymLink>> {
        &self.links
    }

    fn get_links_mut(&mut self) -> &mut Vec<Box<dyn SymLink>> {
        &mut self.links
    }

    fn get_name(&self) -> &str {
        ""
    }
}

pub struct VfsDir {
    files: Vec<Box<dyn File>>,
    directories: Vec<Box<dyn Directory>>,
    links: Vec<Box<dyn SymLink>>,
    permissions: Permissions,
    name: String,
}

impl Directory for VfsDir {
    fn get_permissions(&self) -> Permissions {
        self.permissions
    }

    fn set_permissions(&mut self, permissions: Permissions) -> Result<(), Errno> {
        self.permissions = permissions;
        Ok(())
    }

    fn get_files(&self) -> &Vec<Box<dyn File>> {
        &self.files
    }

    fn get_files_mut(&mut self) -> &mut Vec<Box<dyn File>> {
        &mut self.files
    }

    fn get_directories(&self) -> &Vec<Box<dyn Directory>> {
        &self.directories
    }

    fn get_directories_mut(&mut self) -> &mut Vec<Box<dyn Directory>> {
        &mut self.directories
    }

    fn get_links(&self) -> &Vec<Box<dyn SymLink>> {
        &self.links
    }

    fn get_links_mut(&mut self) -> &mut Vec<Box<dyn SymLink>> {
        &mut self.links
    }

    fn get_name(&self) -> &str {
        &self.name
    }

    fn set_name(&mut self, name: &str) -> Result<(), Errno> {
        self.name = name.to_string();
        Ok(())
    }
}

/// makes a directory in the vfs
pub fn vfs_mkdir(path: &str) {
    let elements = path.split('/').collect::<Vec<_>>();

    fn make_dir(elements: &Vec<&str>, extent: usize) {
        if extent > elements.len() {
            return;
        }

        let mut partial = elements[0..extent].to_vec();

        let dirname = partial.pop().unwrap().to_string();

        let path = partial.join("/");

        let dir = get_directory_from_path(unsafe { ROOT_DIR.as_mut().unwrap() }, &path).unwrap();
        
        let permissions = dir.get_permissions();

        let should_make = || {
            for dir2 in dir.get_directories() {
                if dir2.get_name() == dirname {
                    return false;
                }
            }
            true
        };

        if !dirname.is_empty() && should_make() {
            dir.get_directories_mut().push(Box::new(VfsDir {
                files: Vec::new(),
                directories: Vec::new(),
                links: Vec::new(),
                permissions,
                name: dirname,
            }));
        }

        make_dir(elements, extent + 1);
    }

    make_dir(&elements, 1);
}

pub struct MountPoint {
    dir: Box<dyn Directory>,
    permissions: Permissions,
    name: String,
}

impl Directory for MountPoint {
    fn get_permissions(&self) -> Permissions {
        self.permissions
    }

    fn set_permissions(&mut self, permissions: Permissions) -> Result<(), Errno> {
        self.permissions = permissions;
        Ok(())
    }

    fn get_owner(&self) -> UserID {
        self.dir.get_owner()
    }

    fn set_owner(&mut self, owner: UserID) -> Result<(), Errno> {
        self.dir.set_owner(owner)
    }

    fn get_group(&self) -> GroupID {
        self.dir.get_group()
    }

    fn set_group(&mut self, group: GroupID) -> Result<(), Errno> {
        self.dir.set_group(group)
    }

    fn get_files(&self) -> &Vec<Box<dyn File>> {
        self.dir.get_files()
    }

    fn get_files_mut(&mut self) -> &mut Vec<Box<dyn File>> {
        self.dir.get_files_mut()
    }

    fn get_directories(&self) -> &Vec<Box<dyn Directory>> {
        self.dir.get_directories()
    }

    fn get_directories_mut(&mut self) -> &mut Vec<Box<dyn Directory>> {
        self.dir.get_directories_mut()
    }

    fn get_links(&self) -> &Vec<Box<dyn SymLink>> {
        self.dir.get_links()
    }

    fn get_links_mut(&mut self) -> &mut Vec<Box<dyn SymLink>> {
        self.dir.get_links_mut()
    }

    fn create_file(&mut self, name: &str, permissions: Permissions) -> Result<(), Errno> {
        self.dir.create_file(name, permissions)
    }

    fn create_directory(&mut self, name: &str, permissions: Permissions) -> Result<(), Errno> {
        self.dir.create_directory(name, permissions)
    }

    fn create_link(&mut self, name: &str, target: &str, permissions: Permissions) -> Result<(), Errno> {
        self.dir.create_link(name, target, permissions)
    }

    fn delete_file(&mut self, name: &str) -> Result<(), Errno> {
        self.dir.delete_file(name)
    }

    fn delete_directory(&mut self, name: &str) -> Result<(), Errno> {
        self.dir.delete_directory(name)
    }

    fn delete_link(&mut self, name: &str) -> Result<(), Errno> {
        self.dir.delete_link(name)
    }

    fn get_name(&self) -> &str {
        &self.name
    }

    fn set_name(&mut self, name: &str) -> Result<(), Errno> {
        self.name = name.to_string();
        Ok(())
    }
}

pub fn add_mount_point(name: &str, tree: Box<dyn Directory>) {
    let dir = get_directory_from_path(unsafe { ROOT_DIR.as_mut().unwrap() }, "/fs").expect("couldn't get filesystem directory");
    let permissions = dir.get_permissions();

    dir.get_directories_mut().push(Box::new(MountPoint {
        dir: tree,
        permissions,
        name: name.to_string(),
    }))
}

pub fn remove_mount_point(name: &str) {
    let dir = get_directory_from_path(unsafe { ROOT_DIR.as_mut().unwrap() }, "/fs").expect("couldn't get filesystem directory");

    let mounts = dir.get_directories_mut();

    for i in 0..mounts.len() {
        if mounts[i].get_name() == name {
            mounts.remove(i);
            break;
        }
    }
}

pub fn add_device(name: &str, tree: Box<dyn Directory>) {
    let dir = get_directory_from_path(unsafe { ROOT_DIR.as_mut().unwrap() }, "/dev").expect("couldn't get device directory");
    let permissions = dir.get_permissions();

    dir.get_directories_mut().push(Box::new(MountPoint { // we can just do this again since it works lmao
        dir: tree,
        permissions,
        name: name.to_string(),
    }))
}

pub fn remove_device(name: &str) {
    let dir = get_directory_from_path(unsafe { ROOT_DIR.as_mut().unwrap() }, "/dev").expect("couldn't get device directory");

    let devices = dir.get_directories_mut();

    for i in 0..devices.len() {
        if devices[i].get_name() == name {
            devices.remove(i);
            break;
        }
    }
}

pub fn read_file(path: &str) -> Result<Vec<u8>, Errno> {
    let file = get_file_from_path(unsafe { ROOT_DIR.as_mut().unwrap() }, path)?;

    let mut buf = vec![0; file.get_size().try_into().unwrap_or(0)];
    file.read_at(buf.as_mut_slice(), 0)?;

    Ok(buf)
}

pub fn init() {
    // create root dir
    unsafe {
        ROOT_DIR = Some(Box::new(VfsRoot {
            files: Vec::new(),
            directories: Vec::new(),
            links: Vec::new(),
        }));
    }

    // create directories
    vfs_mkdir("/dev");
    vfs_mkdir("/proc");
    vfs_mkdir("/fs");

    // add console device
    add_device("console", crate::console::make_console_device());

    // mount initrd
    if let Some(initrd) = crate::platform::get_initrd() {
        add_mount_point("initrd", super::tar::make_tree(TarIterator::new(initrd)));
    }

    //super::tree::print_tree(unsafe { ROOT_DIR.as_ref().unwrap() });
}
