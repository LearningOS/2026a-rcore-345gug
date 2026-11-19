use super::{
    block_cache_sync_all, get_block_cache, BlockDevice, DirEntry, DiskInode, DiskInodeType,
    EasyFileSystem, DIRENT_SZ,
};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::{Mutex, MutexGuard};
/// Virtual filesystem layer over easy-fs
pub struct Inode {
    block_id: usize,
    block_offset: usize,
    fs: Arc<Mutex<EasyFileSystem>>,
    block_device: Arc<dyn BlockDevice>,
}

impl Inode {
    /// Create a vfs inode
    pub fn new(
        block_id: u32,
        block_offset: usize,
        fs: Arc<Mutex<EasyFileSystem>>,
        block_device: Arc<dyn BlockDevice>,
    ) -> Self {
        Self {
            block_id: block_id as usize,
            block_offset,
            fs,
            block_device,
        }
    }
    /// Call a function over a disk inode to read it
    fn read_disk_inode<V>(&self, f: impl FnOnce(&DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .read(self.block_offset, f)
    }
    /// Call a function over a disk inode to modify it
    fn modify_disk_inode<V>(&self, f: impl FnOnce(&mut DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .modify(self.block_offset, f)
    }
    /// Find the directory entry of `name` under a disk inode.
    ///
    /// Returns `(entry_index, inode_id)` so that the caller can either get the
    /// inode id or remove the directory entry.
    fn find_inode_entry(&self, name: &str, disk_inode: &DiskInode) -> Option<(usize, u32)> {
        // assert it is a directory
        assert!(disk_inode.is_dir());
        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
        let mut dirent = DirEntry::empty();
        for i in 0..file_count {
            assert_eq!(
                disk_inode.read_at(DIRENT_SZ * i, dirent.as_bytes_mut(), &self.block_device,),
                DIRENT_SZ,
            );
            if dirent.name() == name {
                return Some((i, dirent.inode_id()));
            }
        }
        None
    }
    /// Find inode under a disk inode by name
    fn find_inode_id(&self, name: &str, disk_inode: &DiskInode) -> Option<u32> {
        self.find_inode_entry(name, disk_inode)
            .map(|(_, inode_id)| inode_id)
    }
    /// Find inode under current inode by name
    pub fn find(&self, name: &str) -> Option<Arc<Inode>> {
        let fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            self.find_inode_id(name, disk_inode).map(|inode_id| {
                let (block_id, block_offset) = fs.get_disk_inode_pos(inode_id);
                Arc::new(Self::new(
                    block_id,
                    block_offset,
                    self.fs.clone(),
                    self.block_device.clone(),
                ))
            })
        })
    }
    /// Create a hard link `name` in the current directory pointing to `inode`.
    ///
    /// 硬链接的本质是在目录中新增一个目录项, 它和已有目录项记录同一个
    /// inode 编号, 因此两个名字最终访问的是同一块磁盘数据. 成功时把目标
    /// inode 的 `nlink` 加 1. 目录项重名时返回 -1.
    pub fn link(&self, name: &str, inode: &Inode) -> isize {
        // 整个 link 操作需要在同一个文件系统锁下完成, 避免目录项和 nlink 不一致
        let mut fs = self.fs.lock();
        // 只有目录才能持有目录项
        if !self.read_disk_inode(|disk_inode| disk_inode.is_dir()) {
            return -1;
        }
        // 本实验只定义了 "链接同名文件" 这一种错误: 新路径已存在则失败.
        // 其它 "新路径已存在" 的情况属于未定义行为, 这里同样保守地返回 -1.
        if self
            .read_disk_inode(|disk_inode| self.find_inode_id(name, disk_inode))
            .is_some()
        {
            return -1;
        }
        // 由目标 inode 在磁盘上的位置反推它的 inode 编号
        let inode_id = fs.get_inode_id(inode.block_id as u32, inode.block_offset);
        // 两个目录项指向同一个磁盘 inode, 硬链接数加一
        inode.modify_disk_inode(|disk_inode| {
            disk_inode.nlink += 1;
        });
        // 在当前目录末尾追加一个指向同一 inode 的目录项
        self.modify_disk_inode(|disk_inode| {
            let file_count = (disk_inode.size as usize) / DIRENT_SZ;
            let new_size = (file_count + 1) * DIRENT_SZ;
            self.increase_size(new_size as u32, disk_inode, &mut fs);
            let dirent = DirEntry::new(name, inode_id);
            disk_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device,
            );
        });
        block_cache_sync_all();
        0
    }

    /// Remove the directory entry `name` from the current directory.
    ///
    /// 本实验简化 unlink: 只删除目录项并把目标 inode 的 `nlink` 减 1,
    /// 即使 `nlink` 减到 0 也不回收 inode 和数据块 (见实验报告 "简化点说明").
    /// 路径不存在时返回 -1.
    pub fn unlink(&self, name: &str) -> isize {
        let fs = self.fs.lock();
        // 只有目录才能持有目录项
        if !self.read_disk_inode(|disk_inode| disk_inode.is_dir()) {
            return -1;
        }
        // 找到待删除目录项的位置以及它指向的 inode
        let Some((entry_index, inode_id)) =
            self.read_disk_inode(|disk_inode| self.find_inode_entry(name, disk_inode))
        else {
            return -1;
        };
        // 目标 inode 的硬链接数减一. 注意: nlink 为 0 时不做任何回收.
        let (block_id, block_offset) = fs.get_disk_inode_pos(inode_id);
        get_block_cache(block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(block_offset, |disk_inode: &mut DiskInode| {
                if disk_inode.nlink > 0 {
                    disk_inode.nlink -= 1;
                }
            });
        // 把后面的目录项依次前移, 覆盖被删除项, 并缩小目录大小.
        // 目录缩小时不会释放已经分配的目录数据块, 这是本实验的简化.
        self.modify_disk_inode(|disk_inode| {
            let file_count = (disk_inode.size as usize) / DIRENT_SZ;
            for i in entry_index + 1..file_count {
                let mut dirent = DirEntry::empty();
                assert_eq!(
                    disk_inode.read_at(i * DIRENT_SZ, dirent.as_bytes_mut(), &self.block_device),
                    DIRENT_SZ,
                );
                disk_inode.write_at(
                    (i - 1) * DIRENT_SZ,
                    dirent.as_bytes(),
                    &self.block_device,
                );
            }
            disk_inode.size -= DIRENT_SZ as u32;
        });
        block_cache_sync_all();
        0
    }

    /// Get `(inode_id, nlink, is_dir)` of the current inode.
    ///
    /// `sys_fstat` 用这三个字段填充 `Stat`: inode 编号、硬链接数、文件类型.
    pub fn get_stat(&self) -> (u32, u32, bool) {
        // 先取得文件系统锁, 保证换算 inode 编号和读取磁盘 inode 时视图一致
        let fs = self.fs.lock();
        let inode_id = fs.get_inode_id(self.block_id as u32, self.block_offset);
        let (nlink, is_dir) = self.read_disk_inode(|disk_inode| (disk_inode.nlink, disk_inode.is_dir()));
        (inode_id, nlink, is_dir)
    }

    /// Increase the size of a disk inode
    fn increase_size(
        &self,
        new_size: u32,
        disk_inode: &mut DiskInode,
        fs: &mut MutexGuard<EasyFileSystem>,
    ) {
        if new_size < disk_inode.size {
            return;
        }
        let blocks_needed = disk_inode.blocks_num_needed(new_size);
        let mut v: Vec<u32> = Vec::new();
        for _ in 0..blocks_needed {
            v.push(fs.alloc_data());
        }
        disk_inode.increase_size(new_size, v, &self.block_device);
    }
    /// Create inode under current inode by name
    pub fn create(&self, name: &str) -> Option<Arc<Inode>> {
        let mut fs = self.fs.lock();
        let op = |root_inode: &DiskInode| {
            // assert it is a directory
            assert!(root_inode.is_dir());
            // has the file been created?
            self.find_inode_id(name, root_inode)
        };
        if self.read_disk_inode(op).is_some() {
            return None;
        }
        // create a new file
        // alloc a inode with an indirect block
        let new_inode_id = fs.alloc_inode();
        // initialize inode
        let (new_inode_block_id, new_inode_block_offset) = fs.get_disk_inode_pos(new_inode_id);
        get_block_cache(new_inode_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(new_inode_block_offset, |new_inode: &mut DiskInode| {
                new_inode.initialize(DiskInodeType::File);
            });
        self.modify_disk_inode(|root_inode| {
            // append file in the dirent
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let new_size = (file_count + 1) * DIRENT_SZ;
            // increase size
            self.increase_size(new_size as u32, root_inode, &mut fs);
            // write dirent
            let dirent = DirEntry::new(name, new_inode_id);
            root_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device,
            );
        });

        let (block_id, block_offset) = fs.get_disk_inode_pos(new_inode_id);
        block_cache_sync_all();
        // return inode
        Some(Arc::new(Self::new(
            block_id,
            block_offset,
            self.fs.clone(),
            self.block_device.clone(),
        )))
        // release efs lock automatically by compiler
    }
    /// List inodes under current inode
    pub fn ls(&self) -> Vec<String> {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            let file_count = (disk_inode.size as usize) / DIRENT_SZ;
            let mut v: Vec<String> = Vec::new();
            for i in 0..file_count {
                let mut dirent = DirEntry::empty();
                assert_eq!(
                    disk_inode.read_at(i * DIRENT_SZ, dirent.as_bytes_mut(), &self.block_device,),
                    DIRENT_SZ,
                );
                v.push(String::from(dirent.name()));
            }
            v
        })
    }
    /// Read data from current inode
    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| disk_inode.read_at(offset, buf, &self.block_device))
    }
    /// Write data to current inode
    pub fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let mut fs = self.fs.lock();
        let size = self.modify_disk_inode(|disk_inode| {
            self.increase_size((offset + buf.len()) as u32, disk_inode, &mut fs);
            disk_inode.write_at(offset, buf, &self.block_device)
        });
        block_cache_sync_all();
        size
    }
    /// Clear the data in current inode
    pub fn clear(&self) {
        let mut fs = self.fs.lock();
        self.modify_disk_inode(|disk_inode| {
            let size = disk_inode.size;
            let data_blocks_dealloc = disk_inode.clear_size(&self.block_device);
            assert!(data_blocks_dealloc.len() == DiskInode::total_blocks(size) as usize);
            for data_block in data_blocks_dealloc.into_iter() {
                fs.dealloc_data(data_block);
            }
        });
        block_cache_sync_all();
    }
}
