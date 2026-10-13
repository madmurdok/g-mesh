from pkg.files import FileHandle


def shut(f: FileHandle):
    f.close()
